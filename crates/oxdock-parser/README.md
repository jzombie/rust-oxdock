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
    println!("OxDock DSL grammar:\n{}", LANGUAGE_SPEC);
}
```

Because the parser is generated from this same file, the “spec” and the
implementation stay in lockstep—the DSL is exactly what the grammar
describes.

Every parse failure is a typed [`ParseError`](src/error.rs) with a machine
readable [`ParseErrorKind`](src/error.rs), a 1-based line number, an optional
column span with source line and caret, the offending text when known, the
expected syntax, and a hint with a concrete example. The contract beginners
can rely on: a syntax error is never reported as `unknown command`. Any line
that starts with a known command or structural keyword (`WITH_IO`, `LET`,
`FOR`, `IF`, `ASYNC`, `AWAIT`, and the rest) always fails as
`invalid syntax for command X` with an explanation of what was expected.
Only a name that matches nothing in any case fails as `unknown command`.

The kinds:

- `PestParse`: the grammar could not tokenize the line at all. Always
  carries line, column span, source line, and caret, plus the expected
  rule set. Lowercase commands land here with an uppercase note.
- `InvalidSyntax { command }`: a known command or keyword with malformed
  arguments, flags, arity, or block shape. Carries the received text, the
  canonical syntax, and a hint.
- `UnknownCommand { name }`: a truly unknown name, with a
  `did you mean` hint when only the case is wrong.
- `Structural { rule }`: guard, block, scope, and structural keyword
  failures. Spans are refined to the exact offending pair, so the caret
  lands on the token (for example the `BOOL` in a bad `FOR` key type),
  never just the statement start.
- `Validation { command }`: arity, flag, and static argument type
  failures for a known command.

String and file parsers always populate line, column span, and source
line. Token stream parsers (`macro_input`) leave the source line empty
and keep integer coordinates instead, since `proc_macro2::Span` carries
positions but not source text. Callers keep their own compiler span for
`syn::Error` conversion. Downstream crates receive `ParseError` by value
(`ParseResult<T>`) and convert to their own error type only at the outer
boundary, so matching on `kind`, `line`, and columns never depends on
message text.

`oxdock-parser` builds the executable steps for the OxDock DSL that powers the
CLI and the embedding macros. The DSL is intentionally compact, but it now has
an explicit grammar so that other tooling (formatters, language servers, IDE
plugins, etc.) can understand scripts without re‑implementing the parser.

## License

`oxdock-parser` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
