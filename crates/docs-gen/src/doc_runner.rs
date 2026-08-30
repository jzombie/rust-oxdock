use anyhow::{Context, Result};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use oxdock_parser::parse_script;
use std::path::Path;

pub struct DocSpec<'a> {
    pub template: Option<&'a Path>,
    pub sections_dir: &'a Path,
    pub output_path: &'a Path,
    pub inherit_keys: &'a [&'a str],
}

pub fn assemble_doc(repo_root: &Path, spec: DocSpec, env: ExecIo) -> Result<()> {
    let out = spec.output_path.display();
    let sec_dir = spec.sections_dir.display();

    let inherit_step = if spec.inherit_keys.is_empty() {
        String::new()
    } else {
        let mut keys = spec.inherit_keys.to_vec();
        keys.sort();
        format!("INHERIT_ENV [{}]\n", keys.join(", "))
    };

    let init_step = match spec.template {
        Some(tmpl) => format!(
            r#"
            WITH_IO [stdout=pipe:base] EXPAND '{}'
            WITH_IO [stdin=pipe:base] WRITE '{out}'
            "#,
            tmpl.display()
        ),
        None => format!(r#"RAW_WRITE '{out}' """#),
    };

    let script = format!(
        r#"
        {inherit_step}{init_step}

        LET $sections = GLOB('{sec_dir}/*.md')
        FOR $f IN $sections {{
            WITH_IO [stdout=pipe:sec] READ $f
            WITH_IO [stdin=pipe:sec] APPEND '{out}'
            APPEND '{out}' "\n"
        }}
        "#
    );

    let root = GuardedPath::new_root(repo_root)?;
    let steps = parse_script(&script).context("failed to parse generated DSL script")?;
    run_steps_with_context_result_with_io(&root, &root, &steps, env)?;

    Ok(())
}
