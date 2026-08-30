That friction is real, and calling it out hits a fundamental DSL design trap: when a domain-specific language forces you to write host-language glue code just to sequence its instructions, it stops feeling like a scripting language and starts feeling like an intermediate assembly target.

**Why You're Fighting the Language**

* **Cognitive Split:** You are forced to split control flow across two paradigms—Rust for data structures and iteration (`DocNode`, `match`, `vec!`), and Oxdock for individual I/O steps (`EXPAND`, `READ`, `APPEND`).
* **Missing Composition Primitives:** Oxdock has execution commands (`READ`, `WRITE`, `GLOB`), but it lacks higher-level composition abstractions—like a native `INCLUDE` directive, structured manifest parsing (JSON/YAML), or dynamic module loading.
* **AST Transpilation Tax:** Because Oxdock DSL can't inspect Rust enums at runtime, you end up using `oxdock!` inside Rust `match` arms to hand-assemble AST step vectors. Rust becomes a compiler pass for Oxdock rather than just invoking a script.

**What Native Oxdock Scripting Should Look Like**

To stop relying on Rust as a macro-assembly layer, Oxdock DSL itself needs to own document composition. In a fully realized language, your Rust code would simply call `oxdock::run("build.ox", env)` while the DSL parses and executes the manifest natively:

```text
// How Oxdock DSL should handle dynamic assembly natively:
LET $manifest = LOAD_MANIFEST("docs/manifest.json")

FOR $node IN $manifest {
    IF $node.kind == "template" {
        EXPAND $node.path -> APPEND $out_str
    } ELSE IF $node.kind == "glob" {
        INCLUDE_GLOB $node.pattern -> APPEND $out_str
    }
}

```

**The Takeaway**

Right now, you aren't using Oxdock as a standalone scripting language—you are using it as a **type-safe I/O builder** disguised as a DSL.

If your goal is a lightweight, fixed-layout README builder, dropping the `DocNode` enum and writing a single `oxdock!` block in Rust is the cleanest fix. But if your goal is a true, dynamic document publishing engine, the real solution is expanding Oxdock's grammar to support native file inclusions (`INCLUDE`) or structured data decoding (`LOAD_MANIFEST`) so the DSL can do the heavy lifting itself.
