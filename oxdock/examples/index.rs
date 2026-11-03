// Games launcher: picks a `.oxfile`/`.oxdock` game from this directory
// with the DSL itself, then runs it.
//
// Run with:
//   cargo run -p oxdock --example index

use oxdock::{Engine, Step, oxdock, parse_script};
use oxdock_fs::{GuardedPath, PathResolver};

fn main() -> anyhow::Result<()> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let examples = GuardedPath::new_root(dir.as_path())?;

    // Picker menu built by the DSL: GLOB discovers the games, PROMPT
    // chooses one, and the answer returns through Engine bindings.
    let picker: Vec<Step> = oxdock! {
        IMPORT [STD]
        LET $games: LIST<STRING> = GLOB("*.ox*")
        LET $count: INT = 0
        FOR $g: STRING IN $games {
            IF TEXT_ENDS_WITH($g, ".oxfile") || TEXT_ENDS_WITH($g, ".oxdock") {
                $count = $count + 1
                ECHO "{{ $count }}: {{ $g }}"
            }
        }
        IF $count == 0 {
            ECHO "No games found beside index.rs."
            EXIT 1
        }
        LET $raw: STRING = ""
        LET $choice: STRING = ""
        WHILE true {
            PROMPT $raw "Choose a game by number?" "1"
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
            FOR $h: STRING IN $games {
                IF TEXT_ENDS_WITH($h, ".oxfile") || TEXT_ENDS_WITH($h, ".oxdock") {
                    $k = $k + 1
                    IF $k == $n {
                        $choice = $h
                    }
                }
            }
            BREAK
        }
        ECHO "Starting {{ $choice }}."
    };

    let output = Engine::new().run_steps(&examples, &picker)?;
    let choice = output
        .bindings
        .get("choice")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("picker did not choose a game"))?;

    let game_path = examples.join(choice)?;
    let resolver = PathResolver::new(examples.as_path(), examples.as_path())?;
    let text = resolver.read_to_string(&game_path)?;
    let game: Vec<Step> = parse_script(&text)?;
    Engine::new().run_steps(&examples, &game)?;
    Ok(())
}
