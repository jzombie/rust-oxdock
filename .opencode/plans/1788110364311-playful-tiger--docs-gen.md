# Plan: Fix docs-gen — Composable Templates, No Domain Coupling

## Problem

1. `template_doc::expand` hardcodes `INHERIT_ENV [CRATE_NAME, CRATE_DESCRIPTION]` — breaks generic design
2. `if append { oxdock! } else { oxdock! }` — macro duplication DRY violation
3. Three separate `runner::run` calls per crate — I/O thrashing
4. Variable expansion broken (missing `INHERIT_ENV` in step list)
5. Stale `docs/crates/` directory names

## Fix 1: `template_doc::expand` — single oxdock! block, no INHERIT_ENV

```rust
pub fn expand(repo_root: &Path, template_path: &str, output_path: &str, mut env: ExecIo, append: bool) -> Result<()> {
    let flag = "OXDOCK_APPEND_MODE";
    if append {
        env.insert_inherit_env(flag, "1");
    }
    let steps = oxdock! {
        [!env:#flag] WITH_IO [stdout=pipe:base] EXPAND #template_str
        [!env:#flag] WITH_IO [stdin=pipe:base] WRITE #out_str

        [env:#flag] WITH_IO [stdout=pipe:base] EXPAND #template_str
        [env:#flag] WITH_IO [stdin=pipe:base] APPEND #out_str
    };
    runner::run(repo_root, steps, env)
}
```

- No `INHERIT_ENV` — variables come from `ExecIo` set by `main.rs`
- No `if/else` branch — single `oxdock!` block, guard-based write mode selection
- Single `runner::run` call

## Fix 2: `section_concat::assemble` — same pattern

```rust
pub fn assemble(repo_root: &Path, glob_pattern: &str, output_path: &str, env: ExecIo, truncate: bool) -> Result<()> {
    let flag = "OXDOCK_TRUNCATE_MODE";
    let mut env = env;
    if truncate {
        env.insert_inherit_env(flag, "1");
    }
    let steps = oxdock! {
        [!env:#flag] RAW_WRITE #out_str ""
        [env:#flag] RAW_WRITE #out_str ""

        LET $sections = GLOB(#pattern_str)
        FOR $f IN $sections {
            WITH_IO [stdout=pipe:sec] READ $f
            WITH_IO [stdin=pipe:sec] APPEND #out_str
            APPEND #out_str "\n"
        }
    };
    runner::run(repo_root, steps, env)
}
```

## Fix 3: In-memory pipeline in `main.rs`

Single `runner::run` per crate. Compose header + body + footer in memory:

```rust
fn generate_crate_docs(repo_root: &Path) -> Result<()> {
    let header = std::fs::read_to_string(repo_root.join("docs/templates/crate-header.md"))?;
    let footer = std::fs::read_to_string(repo_root.join("docs/templates/crate-footer.md"))?;

    for member in &members {
        let meta = parse_cargo_metadata(&cargo_toml)?;
        let mut env = ExecIo::new();
        env.insert_inherit_env("CRATE_NAME", &meta.name);
        env.insert_inherit_env("CRATE_DESCRIPTION", &meta.description);

        // Expand header with variables
        let expanded_header = expand_template(&header, &env)?;

        // Collect body sections from docs/crates/{name}/
        let sections_dir = repo_root.join("docs/crates").join(&meta.name);
        let body = if sections_dir.exists() {
            collect_sections(&sections_dir)?
        } else {
            String::new()
        };

        // Expand footer with variables
        let expanded_footer = expand_template(&footer, &env)?;

        // Single write
        let out_path = repo_root.join(member).join("README.md");
        std::fs::write(&out_path, format!("{expanded_header}\n{body}\n{expanded_footer}"))?;
    }
}
```

No three-stage disk I/O. Single write per crate.

## Fix 4: Rename stale dirs + audit references

```
mv docs/crates/oxdock-buildtime-helpers docs/crates/oxdock-build
mv docs/crates/oxdock-buildtime-macros docs/crates/oxdock-macros
```

## Files Modified

| File | Change |
|------|--------|
| `crates/docs-gen/src/template_doc.rs` | Remove INHERIT_ENV, use guard-based append, single oxdock! block |
| `crates/docs-gen/src/section_concat.rs` | Same pattern for truncate |
| `crates/docs-gen/src/main.rs` | In-memory pipeline, single write per crate |
| `docs/templates/crate-header.md` | New |
| `docs/templates/crate-footer.md` | New |
| `docs/templates/crate-readme.md` | Delete |
| `docs/crates/oxdock-buildtime-helpers/` | Rename → `oxdock-build/` |
| `docs/crates/oxdock-buildtime-macros/` | Rename → `oxdock-macros/` |

## Verification

1. `cargo check -p docs-gen`
2. `cargo run -p docs-gen` — variables expand, single write per crate
3. No content loss vs original READMEs
