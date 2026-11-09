//! Pipe plus fresh interpreter: tracer order through the child.
//!
//! Seeds the real `src/render.oxfile`, a tracer hook, and a parent
//! script that pipes a hand-written child through `PIPE_RUN`. The
//! child calls tracer FUNCs in order; the parent asserts the output
//! markers arrive in that order, proving the pipe executes the child
//! with a fresh interpreter. `RENDER_ALL` byte layout is covered by
//! `dispatcher.rs`; end-to-end docs renders are covered by `render.rs`.

use oxdock_core::{Engine, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver};

fn interpreter() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let debug = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable has a target dir");
    #[cfg(windows)]
    let candidate = debug.join("oxdock-render-child.exe");
    #[cfg(not(windows))]
    let candidate = debug.join("oxdock-render-child");
    assert!(
        candidate.is_file(),
        "pipe-child binary missing at {}: run `cargo build -p oxdock-render` first",
        candidate.display()
    );
    candidate.to_string_lossy().into_owned()
}

fn engine(root: &GuardedPath) -> Engine<oxdock_process::DefaultProcessManager> {
    let mut io = ExecIo::new();
    io.set_interpreter(interpreter());
    io.insert_inherit_env(
        oxdock_fs::env::oxdock::WORKSPACE_ROOT,
        root.as_path().to_string_lossy().into_owned(),
    );
    Engine::new().with_io(io)
}

fn seed(root: &GuardedPath, resolver: &PathResolver) {
    for (rel, contents) in [
        ("render.oxfile", include_str!("../src/render.oxfile")),
        ("types.oxfile", include_str!("../src/types.oxfile")),
        (
            "hook.oxfile",
            indoc::indoc! {r#"
                IMPORT [STD]
                FUNC TRACE_A($x: STRING) {
                    RETURN "{{ $x }}+A"
                }
                FUNC TRACE_B($x: STRING) {
                    RETURN "{{ $x }}+B"
                }
                EXPORT TRACE_A
                EXPORT TRACE_B
            "#},
        ),
    ] {
        let path = root.join(rel).expect("join seed");
        resolver
            .write_file(&path, contents.as_bytes())
            .expect("write seed");
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the real interpreter binary")]
fn tracer_stages_run_through_pipe_in_order() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    // Hand-written child: imports the tracer hook, calls A then B,
    // echoes the trace. PIPE_RUN pipes it through the fresh child
    // binary; the output markers prove execution order.
    //
    // The child stays an `indoc` raw string, not an `oxdock!` block:
    // it is data fed to the DSL unit under test (`PIPE_RUN`), and
    // its inner quotes would need Rust-escaping inside macro tokens
    // (`\"` on every quote), which reads worse than the one `\{{`
    // escape the raw string needs.
    let parent = indoc::indoc! {r#"
        INHERIT_ENV [OXDOCK_WORKSPACE_ROOT]
        IMPORT ["render.oxfile" AS C]
        LET $mod: STRING = """
        INHERIT_ENV [RENDER_CONFIG, OXDOCK_WORKSPACE_ROOT]
        WORKSPACE LOCAL
        IMPORT ["hook.oxfile" AS H0]
        LET $x: STRING = "start"
        $x = H0::TRACE_A($x)
        $x = H0::TRACE_B($x)
        ECHO "trace=\{{ $x }}"
        """
        LET $wsroot: STRING = "{{ env:OXDOCK_WORKSPACE_ROOT }}"
        LET $out: STRING = C::PIPE_RUN($mod, $wsroot, "render.json")
    "#};
    let output = engine(&root)
        .run_script(&root, parent)
        .expect("pipeline runs");
    let out = output
        .bindings
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        out.contains("trace=start+A+B"),
        "tracer ran A then B in the child: {out}"
    );
}
