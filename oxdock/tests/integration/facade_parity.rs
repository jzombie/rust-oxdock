//! Facade parity: everything reachable through `oxdock::` must behave
//! identically to `oxdock-cli`, and the re-exported macros must expand
//! against the re-exported support crates without a direct `oxdock-parser`
//! dependency in the consumer.

#[cfg(feature = "cli")]
use oxdock::{EndpointFlags, Options, ScriptSource, execute_with_result};
use oxdock::{oxdock, oxdock_parser};
#[cfg(feature = "cli")]
use oxdock_fs::{GuardedPath, PathResolver};

#[cfg(feature = "cli")]
#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn facade_execute_with_result_runs_script() {
    let workspace = GuardedPath::tempdir().expect("tempdir");
    let workspace_root = workspace.as_guarded_path().clone();
    let script_path = workspace_root.join("script.txt").expect("script path");
    let resolver =
        PathResolver::new(workspace_root.as_path(), workspace_root.as_path()).expect("resolver");
    resolver
        .write_file(&script_path, b"WRITE out.txt hi")
        .expect("write script");
    let opts = Options {
        script: ScriptSource::Path(script_path),
        shell: false,
        endpoints: EndpointFlags::default(),
        remote_serve: false,
        remotes: Vec::new(),
    };
    let result = execute_with_result(opts, workspace_root).expect("execute");
    let snapshot = result
        .snapshot_path()
        .expect("default WRITE materializes the snapshot");
    assert_eq!(snapshot, &result.final_cwd);
    let temp_resolver = PathResolver::new(snapshot.root(), snapshot.root()).expect("resolver");
    let out = snapshot.join("out.txt").expect("out path");
    let contents = temp_resolver.read_to_string(&out).expect("read out");
    assert_eq!(contents.trim(), "hi");
}

#[test]
fn facade_oxdock_macro_expands_against_reexported_parser() {
    // `oxdock!` emits absolute `oxdock_parser::...` paths; this only compiles
    // when the facade's unconditional `oxdock-parser` dependency (re-exported
    // as `oxdock::oxdock_parser`) is in the consumer's extern prelude.
    // Types are referenced through the re-exported crate so this test also
    // passes on `--no-default-features` (macros-only) builds.
    let steps: Vec<oxdock_parser::Step> = oxdock! {
        WRITE out.txt hi
    };
    assert_eq!(steps.len(), 1);
    assert!(
        matches!(&steps[0].kind, oxdock_parser::StepKind::Write { .. }),
        "expected WRITE step, got {:?}",
        steps[0].kind
    );
}
