//! Pipe plus fresh interpreter across all five lifecycle stages.
//!
//! Seeds the real `src/compose.oxfile`, one hook plugin, and a parent
//! script that runs every stage in dispatcher order through the pipe.
//! The hook fires inside the child for INGEST; every other stage
//! passes context through untouched. Needs the real CLI binary next
//! to the test executable (`cargo build -p oxdock` first).

use oxdock_core::{Engine, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver};

fn interpreter() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let debug = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable has a target dir");
    let candidate = debug.join("oxdock");
    assert!(
        candidate.is_file(),
        "interpreter binary missing at {}: run `cargo build -p oxdock` first",
        candidate.display()
    );
    candidate.to_string_lossy().into_owned()
}

fn engine() -> Engine<oxdock_process::DefaultProcessManager> {
    let mut io = ExecIo::new();
    io.set_interpreter(interpreter());
    Engine::new().with_io(io)
}

fn seed(root: &GuardedPath, resolver: &PathResolver) {
    for (rel, contents) in [
        ("compose.oxfile", include_str!("../src/compose.oxfile")),
        (
            "dispatcher.oxfile",
            include_str!("../src/dispatcher.oxfile"),
        ),
        (
            "hook.oxfile",
            indoc::indoc! {r#"
                IMPORT [STD]
                TYPE CTX = MAP<text: STRING>
                FUNC INGEST($ctx: CTX) {
                    LET $t: STRING = $ctx.text
                    RETURN {text: "{{ $t }}+ingest"}
                }
                EXPORT INGEST
                EXPORT TYPE CTX
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
fn all_stages_run_through_pipe_in_dispatcher_order() {
    let temp = GuardedPath::tempdir().expect("tempdir");
    let root = temp.as_guarded_path().clone();
    let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
    seed(&root, &resolver);

    // Order lives only in dispatcher RUN_LIFECYCLE: the child the
    // composer builds statically imports the dispatcher and calls
    // D::RUN_LIFECYCLE, whose bare phase calls resolve to the
    // plugin-populated implementations in the child scope.
    let parent = indoc::indoc! {r#"
        IMPORT ["compose.oxfile" AS C]
        TYPE CTX = MAP<text: STRING>
        TYPE PLUGIN = MAP<path: STRING, alias: STRING, hooks: LIST<STRING>>
        LET $plugins: LIST<PLUGIN> = [{path: "hook.oxfile", alias: "H0", hooks: ["INGEST"]}]
        LET $ctx: CTX = {text: "hello"}
        LET $mod: STRING = C::COMPOSE_FIVE($plugins, "dispatcher.oxfile")
        LET $final: CTX = C::PIPE_RUN($mod, $ctx)
        LET $text: STRING = $final.text
    "#};
    let output = engine().run_script(&root, parent).expect("pipeline runs");
    assert_eq!(
        output.bindings.get("text").and_then(|v| v.as_str()),
        Some("hello+ingest"),
        "INGEST hook ran in the child; other stages passed through",
    );
}
