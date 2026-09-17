# oxdock-func-macro

Host export attribute macros for OxDock DSL native and host functions and types.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Two attribute macros cover the two host extension points, and both derive from the annotated item plus its doc comments with no registration boilerplate.

`#[oxdock_func]` turns a Rust function into a DSL callable. The macro derives a registration marker (the UpperCamelCase of the function name) implementing `OxDockFn`; group markers into a `HostModule` for `Engine::register_module`. `#[oxdock_func(pure)]` selects context free functions, which run on both the AST and the compiled RPN math paths. Plain `#[oxdock_func]` selects stateful functions taking `cx: &mut StepCtx<P>` first. The DSL name defaults to the uppercased Rust name and the declared return type comes from `returns = "..."`. Every builtin dogfoods the same derivation.

`#[oxdock_type]` turns a Rust struct into a DSL payload type with one canonical descriptor singleton. The default heap mode stores the value behind an exclusively owned thin pointer. `shared` stores it in a reference counted buffer, so cloning bumps a count instead of copying and mutation detaches first. `inline` stores `Copy` scalars fitting in 64 bits directly in the word with zero allocation. Hosts mint words with `Value::mint_heap`, `Value::mint_heap_shared`, or `Value::mint_inline`, and read them back with `Value::read_heap`, `Value::read_heap_mut`, or `Value::read_inline`.

See the [runnable example](https://github.com/jzombie/rust-oxdock/blob/main/crates/oxdock-core/src/exec/engine.rs) in `oxdock-core`.

## License

`oxdock-func-macro` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
