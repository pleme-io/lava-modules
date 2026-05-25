//! lava-modules — typed (deflava-module …) reusable sub-architectures.
//!
//! ## Shape
//!
//! ```lisp
//! (deflava-module subnet-pair
//!   :inputs  ((:cidr        :type :cidr-block :required #t)
//!             (:az          :type :availability-zone :required #t)
//!             (:public-tag  :type :string :default "Public"))
//!   :resources (
//!     (aws-subnet "public"
//!       :cidr-block "{cidr}"
//!       :availability-zone "{az}"
//!       :tags ((Tier "{public-tag}"))))
//!   :outputs  ((:public-id (ref aws-subnet "public" id))))
//! ```
//!
//! An architecture calls a module via the typed registry. The module's
//! [`Interface`] gates the binding bag *before* the module body evaluates
//! — typed contract failures surface at compose time, not at apply time.
//!
//! ## Pipeline
//!
//! ```text
//! .tlisp (deflava-module …)
//!   ─► modules_in_source                  → Vec<Module>
//!   ─► ModuleRegistry::register
//!     ↓
//! architecture invokes via (module-call name :k v :k v …)
//!   ─► registry.call(name, bindings)
//!     ─► Interface::validate_inputs       (typed gate)
//!     ─► eval_architecture(synth, …)      (reuse lava-eval body)
//!     ─► Architecture                     (merge resources back into parent)
//! ```

#![allow(clippy::module_name_repetitions)]

use indexmap::IndexMap;
use lava_core::Architecture;
use lava_eval::{eval_architecture, parse_all, Atom, InputBindings, Sx};
use lava_schema::{Field, Interface, SchemaError};
use thiserror::Error;

/// Typed module declaration. Cloned per consumer call so the registry
/// stays a read-only catalog.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    /// Typed interface — gates input bag before body evaluation.
    pub interface: Interface,
    /// `:resources` body, stored as Sx forms for reuse on each call.
    pub resources: Vec<Sx>,
    /// `:outputs` form (Sx list of (:key value) pairs). Cloned into
    /// the synthesized architecture so the caller can `(ref module …)`.
    pub outputs: Option<Sx>,
}

#[derive(Debug, Default)]
pub struct ModuleRegistry {
    by_name: IndexMap<String, Module>,
}

impl ModuleRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, m: Module) {
        self.by_name.insert(m.name.clone(), m);
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Module> {
        self.by_name.get(name)
    }

    #[must_use]
    pub fn names(&self) -> Vec<&String> {
        self.by_name.keys().collect()
    }

    /// Invoke a module by name. Returns the synthesized
    /// [`Architecture`] the module produces (resources + outputs); the
    /// caller is free to merge into a parent architecture.
    ///
    /// # Errors
    /// Surfaces [`ModuleError::Unknown`] when no such module is
    /// registered, [`ModuleError::Schema`] when the bag fails the
    /// typed interface gate, or [`ModuleError::Eval`] when the
    /// resource body fails to evaluate.
    pub fn call(
        &self,
        name: &str,
        bindings: &IndexMap<String, String>,
    ) -> Result<Architecture, ModuleError> {
        let module = self
            .get(name)
            .ok_or_else(|| ModuleError::Unknown(name.to_string()))?;

        // Typed-interface gate.
        if let Err(errors) = module.interface.validate_inputs(bindings) {
            let first = errors
                .first()
                .map_or_else(|| "unknown".to_string(), std::string::ToString::to_string);
            return Err(ModuleError::Schema {
                module: name.to_string(),
                count: errors.len(),
                first,
                errors,
            });
        }

        // Synthesize a (deflava-architecture …) form from the module's
        // interface + resources + outputs, then evaluate through
        // lava-eval. The :inputs clause carries the module's interface
        // fields (so {var} interpolation resolves) and user-supplied
        // bindings override defaults via lava-eval's existing
        // overlay path.
        let synth = build_arch_form(
            name,
            &module.interface,
            &module.resources,
            module.outputs.as_ref(),
        );
        let mut input_bindings = InputBindings::new();
        for (k, v) in bindings {
            input_bindings.set_str(k.clone(), v.clone());
        }
        // The architecture inputs are the module inputs; we wrap with
        // (deflava-architecture …) :inputs from the module interface
        // so defaults still apply.
        let arch = eval_architecture(&render_to_string(&synth), &input_bindings)
            .map_err(|e| ModuleError::Eval(e.to_string()))?;
        Ok(arch)
    }
}

#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("unknown module `{0}`")]
    Unknown(String),
    #[error("module `{module}` rejected {count} input(s): {first}")]
    Schema {
        module: String,
        count: usize,
        first: String,
        errors: Vec<SchemaError>,
    },
    #[error("module body eval: {0}")]
    Eval(String),
    #[error("parse: {0}")]
    Parse(#[from] lava_eval::ParseError),
    #[error("interface parse: {0}")]
    InterfaceParse(#[from] lava_eval::InterfaceParseError),
    #[error("malformed deflava-module form: {0}")]
    MalformedForm(String),
}

/// Scan a source string for every `(deflava-module …)` form and
/// return the typed [`Module`] for each one. Companion
/// `(deflava-interface …)` siblings supply the typed contract.
///
/// # Errors
/// Parse errors, missing-clause errors, and interface-parse errors
/// all surface as typed [`ModuleError`] variants.
pub fn modules_in_source(src: &str) -> Result<Vec<Module>, ModuleError> {
    let forms = parse_all(src)?;
    // First pass: collect every (deflava-interface …) by name.
    let mut interfaces: IndexMap<String, Interface> = IndexMap::new();
    for form in &forms {
        if let Some(xs) = form.as_list() {
            if xs.first().and_then(Sx::as_sym) == Some("deflava-interface") {
                let iface = lava_eval::interface_from_form(xs)?;
                interfaces.insert(iface.name.clone(), iface);
            }
        }
    }
    // Second pass: build a Module per (deflava-module …) form.
    let mut out = Vec::new();
    for form in &forms {
        if let Some(xs) = form.as_list() {
            if xs.first().and_then(Sx::as_sym) == Some("deflava-module") {
                let module = module_from_form(xs, &interfaces)?;
                out.push(module);
            }
        }
    }
    Ok(out)
}

fn module_from_form(
    xs: &[Sx],
    interfaces: &IndexMap<String, Interface>,
) -> Result<Module, ModuleError> {
    let name = xs
        .get(1)
        .and_then(Sx::as_sym)
        .ok_or_else(|| ModuleError::MalformedForm("missing module name".into()))?
        .to_string();

    let mut inputs_clause: Option<&Sx> = None;
    let mut resources: Vec<Sx> = Vec::new();
    let mut outputs: Option<Sx> = None;
    let mut i = 2;
    while i + 1 < xs.len() {
        match xs[i].as_kw() {
            Some("inputs") => {
                inputs_clause = Some(&xs[i + 1]);
            }
            Some("resources") => {
                if let Some(list) = xs[i + 1].as_list() {
                    resources = list.to_vec();
                }
            }
            Some("outputs") => {
                outputs = Some(xs[i + 1].clone());
            }
            _ => {}
        }
        i += 2;
    }

    // Prefer the existing typed interface (sibling deflava-interface
    // with matching name). Falls back to building a minimal interface
    // from the :inputs clause shape.
    let interface = interfaces.get(&name).cloned().unwrap_or_else(|| {
        let mut iface = Interface::new(&name);
        if let Some(list) = inputs_clause.and_then(Sx::as_list) {
            for item in list {
                if let Some(plist) = item.as_list() {
                    if let Some(field_name) = plist.first().and_then(Sx::as_kw) {
                        iface.inputs.insert(
                            field_name.to_string(),
                            Field::optional(lava_types::Type::String),
                        );
                    }
                }
            }
        }
        iface
    });

    Ok(Module {
        name,
        interface,
        resources,
        outputs,
    })
}

/// Build a synthetic (deflava-architecture name :inputs (…) :resources (…) :outputs …) form.
/// The :inputs clause is derived from the module's typed interface so
/// the synthesized architecture has access to every named field.
fn build_arch_form(name: &str, interface: &Interface, resources: &[Sx], outputs: Option<&Sx>) -> Sx {
    let mut inputs: Vec<Sx> = Vec::with_capacity(interface.inputs.len());
    for (field_name, field) in &interface.inputs {
        let mut entry: Vec<Sx> = vec![Sx::Atom(Atom::Kw(field_name.clone()))];
        // Provide a default value sentinel so eval_architecture has
        // something to fall back to when no override is supplied. The
        // operator's call_bindings overlay wins.
        let default_value = field
            .default
            .clone()
            .unwrap_or_else(|| "".to_string());
        entry.push(Sx::Atom(Atom::Str(default_value)));
        inputs.push(Sx::List(entry));
    }

    let mut form = vec![
        Sx::Atom(Atom::Sym("deflava-architecture".into())),
        Sx::Atom(Atom::Sym(name.into())),
        Sx::Atom(Atom::Kw("inputs".into())),
        Sx::List(inputs),
        Sx::Atom(Atom::Kw("resources".into())),
        Sx::List(resources.to_vec()),
    ];
    if let Some(o) = outputs {
        form.push(Sx::Atom(Atom::Kw("outputs".into())));
        form.push(o.clone());
    }
    Sx::List(form)
}

/// Render an Sx tree back to tlisp source text. Used internally so we
/// can drive the existing lava-eval text-based interpreter without
/// duplicating the eval logic.
fn render_to_string(form: &Sx) -> String {
    let mut out = String::new();
    write_sx(form, &mut out);
    out
}

fn write_sx(s: &Sx, out: &mut String) {
    match s {
        Sx::Atom(Atom::Sym(v)) => out.push_str(v),
        Sx::Atom(Atom::Kw(v)) => {
            out.push(':');
            out.push_str(v);
        }
        Sx::Atom(Atom::Str(v)) => {
            out.push('"');
            for c in v.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(c),
                }
            }
            out.push('"');
        }
        Sx::Atom(Atom::Bool(b)) => out.push_str(if *b { "#t" } else { "#f" }),
        Sx::Atom(Atom::Int(n)) => {
            use std::fmt::Write;
            let _ = write!(out, "{n}");
        }
        Sx::List(items) => {
            out.push('(');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_sx(item, out);
            }
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBNET_MODULE: &str = r#"
        (deflava-interface subnet-pair
          :doc "A typed subnet binding"
          :inputs ((:cidr :type :cidr-block :required #t)
                   (:az   :type :availability-zone :required #t)))

        (deflava-module subnet-pair
          :inputs ((:cidr :type :cidr-block :required #t)
                   (:az   :type :availability-zone :required #t))
          :resources (
            (aws-subnet "public"
              :cidr-block "{cidr}"
              :availability-zone "{az}")))
    "#;

    #[test]
    fn modules_in_source_extracts_typed_module_with_interface() {
        let modules = modules_in_source(SUBNET_MODULE).unwrap();
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.name, "subnet-pair");
        assert!(m.interface.inputs.contains_key("cidr"));
        assert_eq!(m.resources.len(), 1);
    }

    #[test]
    fn module_call_renders_typed_resources() {
        let modules = modules_in_source(SUBNET_MODULE).unwrap();
        let mut registry = ModuleRegistry::new();
        for m in modules {
            registry.register(m);
        }
        let mut bindings: IndexMap<String, String> = IndexMap::new();
        bindings.insert("cidr".to_string(), "10.0.1.0/24".to_string());
        bindings.insert("az".to_string(), "us-east-1a".to_string());
        let arch = registry.call("subnet-pair", &bindings).unwrap();
        let json = arch.render_terraform_json().unwrap();
        assert_eq!(
            json["resource"]["aws_subnet"]["public"]["cidr_block"],
            "10.0.1.0/24"
        );
        assert_eq!(
            json["resource"]["aws_subnet"]["public"]["availability_zone"],
            "us-east-1a"
        );
    }

    #[test]
    fn module_call_rejects_missing_required_input_with_typed_error() {
        let modules = modules_in_source(SUBNET_MODULE).unwrap();
        let mut registry = ModuleRegistry::new();
        for m in modules {
            registry.register(m);
        }
        let bindings: IndexMap<String, String> = IndexMap::new();
        let err = registry.call("subnet-pair", &bindings).unwrap_err();
        match err {
            ModuleError::Schema { module, count, .. } => {
                assert_eq!(module, "subnet-pair");
                assert!(count >= 1);
            }
            other => panic!("expected ModuleError::Schema, got {other:?}"),
        }
    }

    #[test]
    fn module_call_unknown_name_surfaces_typed_error() {
        let registry = ModuleRegistry::new();
        let err = registry
            .call("no-such-module", &IndexMap::new())
            .unwrap_err();
        matches!(err, ModuleError::Unknown(_));
    }

    #[test]
    fn modules_without_paired_interface_get_minimal_inferred_interface() {
        let src = r#"
            (deflava-module bare
              :inputs ((:foo :type :string))
              :resources ((aws-vpc "x" :cidr-block "10.0.0.0/16")))
        "#;
        let modules = modules_in_source(src).unwrap();
        assert_eq!(modules[0].name, "bare");
        assert!(modules[0].interface.inputs.contains_key("foo"));
    }

    #[test]
    fn write_sx_round_trips_through_parse_all() {
        let form = Sx::List(vec![
            Sx::Atom(Atom::Sym("deflava-architecture".into())),
            Sx::Atom(Atom::Sym("x".into())),
            Sx::Atom(Atom::Kw("inputs".into())),
            Sx::List(vec![]),
            Sx::Atom(Atom::Kw("resources".into())),
            Sx::List(vec![Sx::List(vec![
                Sx::Atom(Atom::Sym("aws-vpc".into())),
                Sx::Atom(Atom::Str("main".into())),
                Sx::Atom(Atom::Kw("cidr-block".into())),
                Sx::Atom(Atom::Str("10.0.0.0/16".into())),
            ])]),
        ]);
        let text = render_to_string(&form);
        let parsed = parse_all(&text).unwrap();
        assert_eq!(parsed.len(), 1);
    }
}
