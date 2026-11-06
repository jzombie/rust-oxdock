// Script launcher: picks a runnable script from this directory with
// the DSL itself, then runs it.
//
// Run with:
//   cargo run -p oxdock --example index

use oxdock::{Engine, GuardedPath, PathResolver, Step, oxdock, parse_script};

fn main() -> anyhow::Result<()> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let examples = GuardedPath::new_root_from_str(&format!("{manifest}/examples"))?;

    // Picker menu built by the DSL: GLOB discovers the scripts, PROMPT
    // chooses one, and the answer returns through Engine bindings. The
    // picker runs without a snapshot so listings see this directory; the
    // picked script below runs lazy like the CLI.
    let picker: Vec<Step> = oxdock! {
        IMPORT [STD]
        LET $exts: LIST<STRING> = [".oxfile", ".oxdock"]
        LET $found: LIST<STRING> = GLOB("*.ox*")
        LET $scripts: LIST<STRING> = []
        LET $ok: BOOL = false
        FOR $f: STRING IN $found {
            $ok = false
            FOR $e: STRING IN $exts {
                IF TEXT_ENDS_WITH($f, $e) {
                    $ok = true
                }
            }
            IF $ok {
                LIST_APPEND $scripts $f
            }
        }
        LET $count: INT = 0
        FOR $s: STRING IN $scripts {
            $count = $count + 1
            ECHO "{{ $count }}: {{ $s }}"
        }
        IF $count == 0 {
            ECHO "No runnable scripts found beside index.rs."
            EXIT 1
        }
        LET $raw: STRING = ""
        LET $choice: STRING = ""
        WHILE true {
            PROMPT $raw "Choose a script by number?" "1"
            IF !TEXT_MATCH($raw, "^[0-9]+$") {
                ECHO "Numbers only."
                CONTINUE
            }
            LET $n: INT = INT($raw)
            IF $n < 1 || $n > $count {
                ECHO "Between 1 and {{ $count }}."
                CONTINUE
            }
            LET $k: INT = 0
            FOR $t: STRING IN $scripts {
                $k = $k + 1
                IF $k == $n {
                    $choice = $t
                }
            }
            BREAK
        }
        ECHO "Starting {{ $choice }}."
    };

    let output = Engine::new().run_steps_without_snapshot(&examples, &picker)?;
    let choice = output
        .bindings
        .get("choice")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("picker did not choose a script"))?;

    let script_path = examples.join(choice)?;
    let resolver = PathResolver::new(examples.as_path(), examples.as_path())?;
    let text = resolver.read_to_string(&script_path)?;
    let script: Vec<Step> = parse_script(&text)?;
    // Same lazy default as the CLI: snapshot-targeted writes land in a
    // temp snapshot, so file-writing scripts never touch this directory.
    Engine::new().run_steps(&examples, &script)?;
    Ok(())
}
