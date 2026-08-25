use oxdock_buildtime_macros::{embed, prepare};
use oxdock_cli::{ExecutionResult, Options, ScriptSource, execute_with_result};
use oxdock_core::{run_steps_with_context_result_with_io, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::parse_script;
use std::error::Error;

// TODO: For the main SnapshotAssets single-file hash, ensure that matches with the metadata hash defined with the SnapshotAssets.

embed! {
    name: SnapshotAssets,
    script: {
        MKDIR data/inner
        WRITE data/inner/a.txt alpha
        WRITE data/b.txt beta
        WITH_IO [stdout=pipe:cap_dir_hash] HASH_SHA256 data
        WITH_IO [stdin=pipe:cap_dir_hash] WRITE dir_hash.txt
        WITH_IO [stdout=pipe:cap_file_hash] HASH_SHA256 data/inner/a.txt
        WITH_IO [stdin=pipe:cap_file_hash] WRITE file_hash.txt

        // Double-check the hash matches on unix system
        // TODO: Gate system cmd execution based on sha256sum detection: https://github.com/jzombie/oxdock-rs/issues/55
        // [platform=unix] {
        //     CAPTURE_TO_FILE system_hash.txt RUN sh -c "if command -v sha256sum >/dev/null 2>&1; then sha256sum data/inner/a.txt | awk '{print $1}'; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 data/inner/a.txt | awk '{print $1}'; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 data/inner/a.txt | awk '{print $2}'; else echo 'no sha256 tool available' >&2; exit 1; fi | tr 'A-F' 'a-f'"
        // }
    },
    out_dir: "prebuilt",
}

prepare! {
    name: SnapshotPrepared,
    script: {
        MKDIR data/inner
        WRITE data/inner/a.txt alpha
        WRITE data/b.txt beta
        WITH_IO [stdout=pipe:cap_dir_hash] HASH_SHA256 data
        WITH_IO [stdin=pipe:cap_dir_hash] WRITE dir_hash.txt
        WITH_IO [stdout=pipe:cap_file_hash] HASH_SHA256 data/inner/a.txt
        WITH_IO [stdin=pipe:cap_file_hash] WRITE file_hash.txt

        // Double-check the hash matches on unix system
        // TODO: Gate system cmd execution based on sha256sum detection: https://github.com/jzombie/oxdock-rs/issues/55
        // [platform=unix] {
        //     CAPTURE_TO_FILE system_hash.txt RUN sh -c "if command -v sha256sum >/dev/null 2>&1; then sha256sum data/inner/a.txt | awk '{print $1}'; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 data/inner/a.txt | awk '{print $1}'; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 data/inner/a.txt | awk '{print $2}'; else echo 'no sha256 tool available' >&2; exit 1; fi | tr 'A-F' 'a-f'"
        // }
    },
    out_dir: "prebuilt_prepare",
}

const SCRIPT: &str = r#"
    MKDIR data/inner
    WRITE data/inner/a.txt alpha
    WRITE data/b.txt beta
    WITH_IO [stdout=pipe:cap_dir_hash] HASH_SHA256 data
    WITH_IO [stdin=pipe:cap_dir_hash] WRITE dir_hash.txt
    WITH_IO [stdout=pipe:cap_file_hash] HASH_SHA256 data/inner/a.txt
    WITH_IO [stdin=pipe:cap_file_hash] WRITE file_hash.txt

    // Double-check the hash matches on unix system
    // TODO: Gate system cmd execution based on sha256sum detection: https://github.com/jzombie/oxdock-rs/issues/55
    // [platform=unix] {
    //     CAPTURE_TO_FILE system_hash.txt RUN sh -c "if command -v sha256sum >/dev/null 2>&1; then sha256sum data/inner/a.txt | awk '{print $1}'; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 data/inner/a.txt | awk '{print $1}'; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 data/inner/a.txt | awk '{print $2}'; else echo 'no sha256 tool available' >&2; exit 1; fi | tr 'A-F' 'a-f'"
    // }
"#;

fn read_dir_hash(resolver: &PathResolver, root: &GuardedPath) -> Result<String, Box<dyn Error>> {
    let hash_path = root.join("dir_hash.txt")?;
    let contents = resolver.read_to_string(&hash_path)?;
    Ok(contents.trim().to_string())
}

fn read_file_hash(resolver: &PathResolver, root: &GuardedPath) -> Result<String, Box<dyn Error>> {
    let hash_path = root.join("file_hash.txt")?;
    let contents = resolver.read_to_string(&hash_path)?;
    Ok(contents.trim().to_string())
}

fn read_manifest_hash(
    resolver: &PathResolver,
    rel_path: &str,
) -> Result<String, Box<dyn Error>> {
    let hash_path = resolver.root().join(rel_path)?;
    let contents = resolver.read_to_string(&hash_path)?;
    Ok(contents.trim().to_string())
}

fn read_optional_hash(
    resolver: &PathResolver,
    root: &GuardedPath,
    name: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let path = root.join(name)?;
    if resolver.entry_kind(&path).is_err() {
        return Ok(None);
    }
    let contents = resolver.read_to_string(&path)?;
    Ok(Some(contents.trim().to_string()))
}

fn main() -> Result<(), Box<dyn Error>> {
    let workspace = GuardedPath::tempdir()?;
    let workspace_root = workspace.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(workspace_root.clone(), workspace_root.clone())?;

    let script_path = workspace_root.join("script.ox")?;
    resolver.write_file(&script_path, SCRIPT.as_bytes())?;

    let opts = Options {
        script: ScriptSource::Path(script_path),
        shell: false,
    };

    let ExecutionResult { tempdir, final_cwd } = execute_with_result(opts, workspace_root.clone())?;
    let cli_resolver = PathResolver::new_guarded(
        tempdir.as_guarded_path().clone(),
        workspace_root.clone(),
    )?;
    let cli_hash = read_dir_hash(&cli_resolver, &final_cwd)?;
    let cli_file_hash = read_file_hash(&cli_resolver, &final_cwd)?;

    let core_temp = GuardedPath::tempdir()?;
    let core_root = core_temp.as_guarded_path().clone();
    let core_resolver = PathResolver::new_guarded(core_root.clone(), workspace_root.clone())?;
    let steps = parse_script(SCRIPT)?;
    let core_final =
        run_steps_with_context_result_with_io(&core_root, &workspace_root, &steps, ExecIo::new())?;
    let core_hash = read_dir_hash(&core_resolver, &core_final)?;
    let core_file_hash = read_file_hash(&core_resolver, &core_final)?;

    let embedded = SnapshotAssets::get("dir_hash.txt").ok_or("missing dir_hash.txt")?;
    let embedded_hash = std::str::from_utf8(embedded.data.as_ref())?
        .trim()
        .to_string();

    assert_eq!(
        cli_hash, core_hash,
        "CLI/core hash mismatch: cli={cli_hash} core={core_hash}"
    );
    assert_eq!(
        cli_hash, embedded_hash,
        "CLI/embed hash mismatch: cli={cli_hash} embed={embedded_hash}"
    );

    let manifest_resolver = PathResolver::from_manifest_env()?;
    let prepared_hash =
        read_manifest_hash(&manifest_resolver, "prebuilt_prepare/dir_hash.txt")?;
    assert_eq!(
        cli_hash, prepared_hash,
        "CLI/prepare hash mismatch: cli={cli_hash} prepare={prepared_hash}"
    );
    assert_eq!(
        cli_file_hash, core_file_hash,
        "CLI/core file hash mismatch: cli={cli_file_hash} core={core_file_hash}"
    );
    assert_ne!(
        cli_hash, cli_file_hash,
        "dir hash should differ from file hash: dir={cli_hash} file={cli_file_hash}"
    );

    // TODO: Enable based on above TODOs
    // #[cfg(unix)]
    // {
    //     if let Some(system_hash) =
    //         read_optional_hash(&cli_resolver, &final_cwd, "system_hash.txt")?
    //     {
    //         assert!(
    //             !system_hash.is_empty(),
    //             "CLI/system file hash missing output"
    //         );
    //         assert_eq!(
    //             system_hash, cli_file_hash,
    //             "CLI/system file hash mismatch: system={system_hash} file={cli_file_hash}"
    //         );
    //     }
    //     if let Some(system_hash) =
    //         read_optional_hash(&core_resolver, &core_final, "system_hash.txt")?
    //     {
    //         assert!(
    //             !system_hash.is_empty(),
    //             "core/system file hash missing output"
    //         );
    //         assert_eq!(
    //             system_hash, core_file_hash,
    //             "core/system file hash mismatch: system={system_hash} file={core_file_hash}"
    //         );
    //     }
    //     if let Some(system_hash) = read_manifest_hash(
    //         &manifest_resolver,
    //         "prebuilt_prepare/system_hash.txt",
    //     )
    //     .ok()
    //     {
    //         let prepared_file_hash =
    //             read_manifest_hash(&manifest_resolver, "prebuilt_prepare/file_hash.txt")?;
    //         assert!(
    //             !system_hash.is_empty(),
    //             "prepare/system file hash missing output"
    //         );
    //         assert_eq!(
    //             system_hash, prepared_file_hash,
    //             "prepare/system file hash mismatch: system={system_hash} file={prepared_file_hash}"
    //         );
    //     }
    // }

    Ok(())
}
