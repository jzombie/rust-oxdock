# oxdock-macros

Proc macros for OxDock: compile-time embedding, runtime AST construction, and DSL processing.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Three macros cover the three ways to use the DSL: `oxdock_embed!` runs a script at compile time and ships the artifacts inside the binary, `oxdock_prepare!` runs the same script without emitting a runtime module, and `oxdock!` builds an inline script into a `Vec<Step>` for runners like `run_steps_with_context_result`. Use `#var` in `oxdock!` to inject Rust values. DSL variables keep their `$var` form.

See the parent repository for the DSL reference and examples: the macros run the same OxDock DSL used by the `oxdock` CLI.

## License

`oxdock-macros` is distributed under the terms of the Apache License (Version 2.0).
