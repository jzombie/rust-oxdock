use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn generate_all(repo_root: &Path) -> Result<()> {
    let crates_dir = repo_root.join("docs/crates");
    if !crates_dir.exists() {
        eprintln!("No docs/crates/ directory found, skipping sub-crate README generation");
        return Ok(());
    }

    let template_path = repo_root.join("docs/templates/crate-readme.md");

    let entries: Vec<PathBuf> = std::fs::read_dir(&crates_dir)
        .with_context(|| format!("failed to read {}", crates_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();

    for crate_dir in entries {
        let crate_name = crate_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let cargo_toml_path = resolve_cargo_toml(repo_root, &crate_name)?;
        if !cargo_toml_path.exists() {
            eprintln!("Skipping {crate_name}: no Cargo.toml found");
            continue;
        }

        match generate_one(&template_path, &crate_dir, &cargo_toml_path, repo_root) {
            Ok(()) => {
                let target_dir = cargo_toml_path.parent().unwrap_or(Path::new("."));
                eprintln!("  Generated {}/README.md", target_dir.display());
            }
            Err(err) => eprintln!("  Warning: failed to generate {crate_name}/README.md: {err:#}"),
        }
    }

    Ok(())
}

fn resolve_cargo_toml(repo_root: &Path, crate_name: &str) -> Result<PathBuf> {
    let candidates = [
        repo_root.join(format!("crates/{crate_name}/Cargo.toml")),
        repo_root.join(format!("crates/sys/{crate_name}/Cargo.toml")),
        repo_root.join(format!("{crate_name}/Cargo.toml")),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }
    Ok(candidates[0].clone())
}

fn generate_one(
    template_path: &Path,
    crate_dir: &Path,
    cargo_toml: &Path,
    repo_root: &Path,
) -> Result<()> {
    use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
    use oxdock_fs::GuardedPath;
    use oxdock_parser::parse_script;

    let metadata = parse_cargo_metadata(cargo_toml)?;
    let source_files = load_source_files(crate_dir, &metadata.name);

    let mut vars = HashMap::new();
    vars.insert("CRATE_NAME".to_string(), metadata.name);
    vars.insert("CRATE_DESCRIPTION".to_string(), metadata.description);
    vars.insert("CRATE_OVERVIEW".to_string(), source_files.overview);
    vars.insert("CRATE_QUICK_START".to_string(), source_files.quick_start);
    vars.insert("CRATE_API".to_string(), source_files.api);

    let target_dir = cargo_toml.parent().context("invalid Cargo.toml path")?;
    let readme_path = target_dir.join("README.md");

    // Sort keys: HashMap iteration order is randomized (SipHash seed),
    // so sorting ensures deterministic INHERIT_ENV key list across runs.
    // Note: This is not a correctness requirement.
    let mut keys: Vec<_> = vars.keys().cloned().collect();
    keys.sort();

    let norm_template = template_path.to_string_lossy().replace('\\', "/");
    let norm_readme = readme_path.to_string_lossy().replace('\\', "/");

    let script = format!(
        "INHERIT_ENV [{}]\nWORKSPACE LOCAL\nWITH_IO [stdout=pipe:out] EXPAND \"{}\"\nWITH_IO [stdin=pipe:out] WRITE \"{}\"",
        keys.join(", "),
        norm_template,
        norm_readme
    );

    let mut io = ExecIo::new();
    for (key, value) in &vars {
        io.insert_inherit_env(key, value);
    }

    let root = GuardedPath::new_root(repo_root)?;
    let temp = GuardedPath::tempdir()?;
    let steps = parse_script(&script)?;

    run_steps_with_context_result_with_io(temp.as_guarded_path(), &root, &steps, io)?;

    Ok(())
}

struct CargoMetadata {
    name: String,
    description: String,
}

fn parse_cargo_metadata(cargo_toml: &Path) -> Result<CargoMetadata> {
    let contents = std::fs::read_to_string(cargo_toml)
        .with_context(|| format!("failed to read {}", cargo_toml.display()))?;
    let doc: toml_edit::DocumentMut = contents.parse().context("failed to parse Cargo.toml")?;

    let name = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let description = doc
        .get("package")
        .and_then(|p| p.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("No description")
        .to_string();

    Ok(CargoMetadata { name, description })
}

struct SourceFiles {
    overview: String,
    quick_start: String,
    api: String,
}

fn load_source_files(crate_dir: &Path, crate_name: &str) -> SourceFiles {
    SourceFiles {
        overview: load_section(crate_dir, "overview.md")
            .unwrap_or_else(|| "Part of the OxDock workspace.".to_string()),
        quick_start: load_section(crate_dir, "quick-start.md").unwrap_or_else(|| {
            format!("See the [API documentation](https://docs.rs/{crate_name}).")
        }),
        api: load_section(crate_dir, "api.md").unwrap_or_else(|| {
            format!("See the [API documentation](https://docs.rs/{crate_name}).")
        }),
    }
}

fn load_section(crate_dir: &Path, filename: &str) -> Option<String> {
    let path = crate_dir.join(filename);
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}
