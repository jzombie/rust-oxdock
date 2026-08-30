# oxdock-parser

Parser and AST definitions for the OxDock DSL.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.
1. The lexer (powered by [`pest`](https://pest.rs/)) tokenizes scripts
   according to `dsl.pest`, handling comments and semicolons along the way.
2. Tokens are fed into the existing `ScriptParser`, which performs guard stack
   combination, scope tracking, and `StepKind` construction (no DSL
   behaviour changed from the previous hand-written line parser).
3. The resulting `Vec<Step>` is consumed by runtimes (CLI, macros, tests, etc.).

All existing semantics—including guard combinations, case sensitivity,
semicolon behaviour, and error messages—remain the same, but they are now
enforced through the shared grammar file.

The grammar lives in [`src/dsl.pest`](src/dsl.pest) and is consumed directly by
the lexer. A copy of the grammar is also exposed at runtime via the
`oxdock_parser::LANGUAGE_SPEC` constant so that downstream tools can embed or
inspect the canonical definition without reaching into the crate filesystem.

```
use oxdock_parser::LANGUAGE_SPEC;

fn dump_grammar() {
    println!("OxDock DSL grammar:\\n{}", LANGUAGE_SPEC);
}
```

Because the parser is generated from this same file, the “spec” and the
implementation stay in lockstep—the DSL is exactly what the grammar
describes.

`oxdock-parser` builds the executable steps for the OxDock DSL that powers the
CLI and the embedding macros. The DSL is intentionally compact, but it now has
an explicit grammar so that other tooling (formatters, language servers, IDE
plugins, etc.) can understand scripts without re‑implementing the parser.

## License

`oxdock-parser` is distributed under the terms of the Apache License (Version 2.0).
