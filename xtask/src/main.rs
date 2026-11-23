use oxdock_macros::oxdock;

fn main() {
    let root = oxdock_fs::discover_workspace_root().expect("find workspace root");

    let steps = oxdock! {
        IMPORT [STD]
        ECHO "building docs"
        RUN ["cargo", "run", "-p", "oxdock-render"]
    };

    oxdock_core::run_steps_with_context_result_with_io(
        &root,
        &root,
        &steps,
        oxdock_core::ExecIo::new(),
    )
    .expect("build docs");
}
