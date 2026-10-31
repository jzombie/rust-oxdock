# oxdock-parser

Parser and AST definitions for the OxDock DSL.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

1. Two frontends lex two distinct input domains into one intermediate
   representation. `lex_str` consumes raw `&str` text (scripts, files,
   rebuilt macro output). `lex_tokens` consumes `rustc`'s `TokenStream`
   (braced `oxdock! { ... }` input, where string literals arrive
   shredded, spacing is normalized, and comments are already gone).
   Both emit the same bespoke IR: a `Vec<Tok>` of words, strings,
   groups, punctuation, newlines, and comment markers with
   gap-adjacency flags. Neither frontend decides statement boundaries.
2. The single parser backend (`parser`) consumes only `[Tok]`
   slices. It splits statements from grammar state, folds guards,
   resolves calls, and constructs `StepKind` values through the shared
   lowering cores. It never sees source text and never sees a
   `TokenStream`.
3. The resulting `Vec<Step>` is consumed by runtimes (CLI, macros, tests, etc.).

The shape is a V converging on `Tok`, not a funnel into `TokenStream`.
`TokenStream` is one input domain among two, never the universal
denominator: the parser only ever sees `Tok`. The convergence is
proven, not assumed: the lexer-agreement oracle (`assert_parity` in
`parser` tests) runs the same script through both lexers into the
same backend on every case and asserts identical steps. Production
braced entry points (`parse_braced_tokens*`) travel via the text
rebuild (`script_from_braced_tokens` into `parse_script`), which the
oracle pins equivalent to lexing the tokens directly.

All existing semantics—including guard combinations, case sensitivity,
semicolon behaviour, and error messages—remain the same, but they are now
enforced through the single backend both frontends share.

The language is defined by two context-free lexers plus one unified
backend, all in this crate. `tok` turns string sources (`lex_str`) and
braced-macro token streams (`lex_tokens`) into a single `Tok` stream:
words, strings, groups, punctuation, newlines, and comment markers with
gap-adjacency flags, never statement decisions. `parser` resolves
statement boundaries from grammar state and lowers to `Step`s; shared
lowering cores (expression folding, call resolution, instruction
tokens, wrapper and REMOTE validators) live in `commands` and
`resolve`, so both frontends behave identically by construction.

```
use oxdock_parser::{lex_str, parse_script, test_lower_mock};

let toks = lex_str("ECHO hi").expect("lexes");
let steps = parse_script("ECHO hi", test_lower_mock::lower).expect("parses");
assert_eq!(steps.len(), 1);
```

Because both pathways share the backend, the “spec” and the
implementation stay in lockstep—the DSL is exactly what the backend
parses.

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

- `PestParse`: low-level syntax the backend never reaches lowering
  for (unbalanced groups, literal scans, lowercase heads). Always
  carries line plus the source line and caret; lowercase commands land
  here with an uppercase note.
- `InvalidSyntax { command }`: a known command or keyword with malformed
  arguments, flags, arity, or block shape. Carries the received text, the
  canonical syntax, and a hint.
- `UnknownCommand { name }`: a truly unknown name, with a
  `did you mean` hint when only the case is wrong.
- `Structural { rule }`: guard, block, scope, and structural keyword
  failures. Spans cover the statement's first line, so the caret marks
  the statement (header interiors such as a bad `FOR` key type name
  their offender in the message); lexer-exact positions such as
  unbalanced groups keep column precision.
- `Validation { command }`: arity, flag, and static argument type
  failures for a known command.

String parsers always populate line and source line with a statement
granularity column span. Token stream parsers (`macro_input`) leave
the source line empty and keep integer coordinates instead, since
`proc_macro2::Span` carries positions but not source text. Callers keep
their own compiler span for `syn::Error` conversion. Downstream crates
receive `ParseError` by value (`ParseResult<T>`) and convert to their
own error type only at the outer boundary, so matching on `kind` and
`line` never depends on message text.

`oxdock-parser` builds the executable steps for the OxDock DSL that powers the
CLI and the embedding macros. The DSL is intentionally compact, but it now has
an explicit grammar so that other tooling (formatters, language servers, IDE
plugins, etc.) can understand scripts without re‑implementing the parser.

## License

`oxdock-parser` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
