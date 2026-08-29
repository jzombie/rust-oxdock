use anyhow::Result;
use oxdock_parser::COMMANDS;
use std::path::Path;

pub fn generate(output: &Path) -> Result<()> {
    let table = generate_table();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, table)?;
    Ok(())
}

fn generate_table() -> String {
    let mut out = String::from("| Command | Syntax |\n| --- | --- |\n");
    for cmd in COMMANDS {
        let keyword = cmd.as_str();
        let syntax = cmd.syntax();
        let escaped_syntax = syntax.replace('|', "\\|");
        let anchor = keyword.to_lowercase();
        out.push_str(&format!(
            "| [`{keyword}`](#{anchor}) | `{escaped_syntax}` |\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_all_commands() {
        let table = generate_table();
        for cmd in COMMANDS {
            let keyword = cmd.as_str();
            assert!(
                table.contains(&format!("`{keyword}`")),
                "missing command {keyword} in generated table"
            );
        }
    }
}
