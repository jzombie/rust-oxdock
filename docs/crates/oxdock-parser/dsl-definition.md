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
