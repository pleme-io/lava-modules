# lava-modules

Typed `(deflava-module …)` reusable sub-architectures for the lava +
tatara-lisp ecosystem. Terraform-module analog.

## Shape

```lisp
(deflava-interface subnet-pair
  :inputs ((:cidr :type :cidr-block :required #t)
           (:az   :type :availability-zone :required #t)))

(deflava-module subnet-pair
  :inputs ((:cidr :type :cidr-block :required #t)
           (:az   :type :availability-zone :required #t))
  :resources (
    (aws-subnet "public"
      :cidr-block "{cidr}"
      :availability-zone "{az}"))
  :outputs ((:public-id (ref aws-subnet "public" id))))
```

## Pipeline

```text
.tlisp (deflava-module …) + (deflava-interface …)
  ─► modules_in_source                  → Vec<Module>
  ─► ModuleRegistry::register
    ↓
architecture invokes via registry.call(name, bindings)
  ─► Interface::validate_inputs           (typed gate)
  ─► synth (deflava-architecture …)       (typed Sx tree, no format!())
  ─► eval_architecture (lava-eval)        (reuses existing interpreter)
  ─► Architecture                          (caller merges resources)
```

## Typed surface

```rust
use lava_modules::{modules_in_source, ModuleRegistry};
use indexmap::IndexMap;

let modules = modules_in_source(&src)?;
let mut registry = ModuleRegistry::new();
for m in modules { registry.register(m); }

let mut bindings = IndexMap::new();
bindings.insert("cidr".to_string(), "10.0.1.0/24".to_string());
bindings.insert("az".to_string(), "us-east-1a".to_string());
let arch = registry.call("subnet-pair", &bindings)?;
let json = arch.render_terraform_json()?;
```

Missing-required-input + unknown-module + body-eval errors all
surface as typed `ModuleError` variants. No format!() of tlisp;
synthesized architecture is built as an Sx tree + rendered through
a typed writer.
