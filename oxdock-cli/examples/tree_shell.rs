// Demo: build a small file tree with an embedded DSL script, then optionally
// drop into an interactive shell inside it.
//
// Run without a shell:
//   cargo run -p oxdock-cli --example tree_shell
// Run and drop into the temp workspace once the script finishes:
//   cargo run -p oxdock-cli --example tree_shell -- --shell

use oxdock_cli::{Options, ScriptSource, execute};
use oxdock_fs::{GuardedPath, PathResolver};

const SCRIPT: &str = r#"
# Build a demo tree to explore.
MKDIR demo/assets
MKDIR demo/logs
WRITE demo/assets/hello.txt hello
LS demo
"#;

fn main() -> anyhow::Result<()> {
    let shell = std::env::args().skip(1).any(|arg| arg == "--shell");

    let temp = GuardedPath::tempdir()?;
    let root = temp.as_guarded_path().clone();

    // Materialize the script inside the guarded temp workspace; the whole
    // workspace (script included) disappears when `temp` drops at end of main.
    let script_path = root.join("script.ox")?;
    let resolver = PathResolver::new(root.as_path(), root.as_path())?;
    resolver.write_file(&script_path, SCRIPT.as_bytes())?;

    println!("temp workspace: {}", temp.display());
    println!("script:{SCRIPT}");
    if shell {
        println!(
            "After the script runs you'll be dropped into the temp workspace. Exit the shell to finish.\n"
        );
    }

    execute(
        Options {
            script: ScriptSource::Path(script_path),
            shell,
        },
        root,
    )
}
