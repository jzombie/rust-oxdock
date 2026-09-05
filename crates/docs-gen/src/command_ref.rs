use anyhow::Result;
use oxdock_core::all_metadata;
#[allow(clippy::disallowed_types)]
use std::path::Path;

#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn generate(output: &Path) -> Result<()> {
    let body = generate_body();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, body)?;
    Ok(())
}

fn generate_body() -> String {
    let mut out = String::new();

    // Structural constructs (parsed by PEG, not CommandSpec)
    out.push_str(&generate_structural_section());

    // Command-specific constructs (from CommandSpec metadata)
    for meta in all_metadata() {
        out.push_str(&format!("### {}\n\n", meta.name));
        out.push_str(&format!("{}\n\n", meta.summary));
        out.push_str(&format!("**Syntax:** `{}`\n\n", meta.syntax));

        if !meta.description.is_empty() {
            out.push_str(&format!("{}\n\n", meta.description));
        }

        if !meta.args.is_empty() {
            out.push_str("**Arguments:**\n\n");
            out.push_str("| Name | Type | Required | Description |\n");
            out.push_str("| --- | --- | --- | --- |\n");
            for arg in meta.args {
                let req = if arg.required { "yes" } else { "no" };
                out.push_str(&format!(
                    "| `{}` | `{}` | {} | {} |\n",
                    arg.name, arg.arg_type, req, arg.description
                ));
            }
            out.push('\n');
        }

        if !meta.flags.is_empty() {
            out.push_str("**Flags:**\n\n");
            out.push_str("| Flag | Type | Description |\n");
            out.push_str("| --- | --- | --- |\n");
            for flag in meta.flags {
                out.push_str(&format!(
                    "| `{}` | {:?} | {} |\n",
                    flag.long, flag.value_type, flag.description
                ));
            }
            out.push('\n');
        }

        if let Some(stream) = meta.default_output {
            out.push_str(&format!("**Output:** {:?}\n\n", stream));
        }

        if !meta.examples.is_empty() {
            out.push_str("**Examples:**\n\n");
            for example in meta.examples {
                out.push_str(&format!("**Example: {}**\n\n", example.name));
                let fence = example
                    .fence_meta
                    .map_or_else(|| "oxdock".to_string(), |m| format!("oxdock {m}"));
                out.push_str(&format!(
                    "```{}\n{}\n```\n\n",
                    fence,
                    example.code.trim_end()
                ));
            }
        }

        out.push('\n');
    }
    out
}

fn generate_structural_section() -> String {
    let mut out = String::new();

    out.push_str("### WITH_IO\n\n");
    out.push_str("Reroutes the standard streams of the next command or, in block form, of every enclosed command. Bindings map streams (`stdin`, `stdout`, `stderr`) to named pipes (`stdout=pipe:name`). Pipe names registered by the host runtime tee structured output elsewhere; a name bound as output can later feed another command's `stdin`, connecting commands without touching the terminal.\n\n");
    out.push_str("**Inline form:** `WITH_IO [bindings] <command>`\n\n");
    out.push_str("**Block form:**\n");
    out.push_str("```oxdock\nWITH_IO [stdout=pipe:log] {\n  ECHO first\n  ECHO second\n}\nWITH_IO [stdin=pipe:log] WRITE captured.txt\n```\n\n");
    out.push_str("Nested blocks stack defaults; inline bindings override inherited ones for their command only; closing a block restores previous wiring.\n\n");

    out.push_str("### INHERIT_ENV\n\n");
    out.push_str("Declares which host environment variables to inherit into the script. Must appear before any other commands and at most once. Without this directive, the script starts with an empty environment.\n\n");
    out.push_str("```oxdock\nINHERIT_ENV [PATH, HOME]\n```\n\n");

    out.push_str("### FOR\n\n");
    out.push_str("Iterates over a list or map. The loop variable receives each element (lists) or value (maps); with two variables, the first receives the key.\n\n");
    out.push_str("```oxdock\nLET $items = [\"a\", \"b\"]\nFOR $item IN $items {\n  ECHO $item\n}\n\nLET $map = {\"x\": 1}\nFOR $k, $v IN $map {\n  ECHO \"$k=$v\"\n}\n```\n\n");

    out.push_str("### IF / ELSE IF / ELSE\n\n");
    out.push_str("Conditional execution. The condition is evaluated as a boolean expression.\n\n");
    out.push_str("```oxdock\nIF true {\n  ECHO yes\n} ELSE {\n  ECHO no\n}\n\nIF false {\n  ECHO skipped\n} ELSE IF true {\n  ECHO fallback\n}\n```\n\n");

    out.push_str("### LET\n\n");
    out.push_str("Assigns a value to a script-local variable. Variables are usable in templates (`{{ $var }}`), guards, and expressions.\n\n");
    out.push_str("```oxdock\nLET $name = \"world\"\nECHO \"hello, {{ $name }}\"\n\nLET $items = [\"a\", \"b\"]\nLET $count = 42\n```\n\n");

    out.push_str("### ASYNC\n\n");
    out.push_str("Runs a command or block of commands in a background thread with subshell isolation. Mutations (ENV, WORKDIR) stay within the block.\n\n");
    out.push_str("**Inline form:** `ASYNC <command...>`\n\n");
    out.push_str("**Block form:**\n");
    out.push_str("```oxdock\nASYNC RUN \"sleep 1\"\n\nASYNC {\n    RUN \"echo first\"\n    RUN \"echo second\"\n}\n```\n\n");

    out.push_str("### LET $var = ASYNC\n\n");
    out.push_str("Spawns a background task and stores a handle in a variable. Use `AWAIT` to wait for completion.\n\n");
    out.push_str("```oxdock\nLET $task = ASYNC {\n    RUN \"cargo build --release\"\n}\nAWAIT $task\n```\n\n");

    out.push_str("### AWAIT\n\n");
    out.push_str("Blocks until the named task completes. Propagates errors if the task failed.\n\n");
    out.push_str("```oxdock\nLET $task = ASYNC RUN \"echo done\"\nAWAIT $task\n```\n\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_all_commands() {
        let body = generate_body();
        for meta in all_metadata() {
            assert!(
                body.contains(&format!("### {}", meta.name)),
                "missing section for command {}",
                meta.name
            );
        }
    }

    #[test]
    fn body_has_structural_constructs() {
        let body = generate_body();
        for construct in &["WITH_IO", "INHERIT_ENV", "FOR", "IF", "LET", "ASYNC", "LET $var = ASYNC", "AWAIT"] {
            assert!(
                body.contains(&format!("### {construct}")),
                "missing section for structural construct {construct}"
            );
        }
    }
}
