1. The lexer (powered by [`pest`](https://pest.rs/)) tokenizes scripts
   according to `dsl.pest`, handling comments and semicolons along the way.
2. Tokens are fed into the existing `ScriptParser`, which performs guard stack
   combination, scope tracking, and `StepKind` construction (no DSL
   behaviour changed from the previous hand-written line parser).
3. The resulting `Vec<Step>` is consumed by runtimes (CLI, macros, tests, etc.).

All existing semantics—including guard combinations, case sensitivity,
semicolon behaviour, and error messages—remain the same, but they are now
enforced through the shared grammar file.
