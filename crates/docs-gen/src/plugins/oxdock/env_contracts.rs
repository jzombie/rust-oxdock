//! Live environment-variable contract reference: rendered from the
//! seed tables in `oxdock_process::env_policy`, so the documented
//! defaults can never drift from the seeded ones. A test pins that
//! every seeded name renders; this renderer only formats.

use anyhow::Result;
use oxdock_markdown_plugin::markdown::escape_table_cell;

/// Render the full contract reference as markdown. Formatting only:
///
/// - the intro is `env_policy::CONTRACT_INTRO_MD`,
/// - the baseline table is `env_policy::describe_baseline_defaults`,
/// - the routing chapter is `env_policy::ROUTING_MD`,
/// - the host-read table is `env_policy::engine_host_reads`,
/// - the enforcement chapter is `env_policy::PRECEDENCE_MD`,
/// - the plumbing table is `oxdock_fs::env::plumbing`.
///
/// Nothing normative lives here; edits belong in the
/// `define_env_contract!` invocation in
/// `crates/sys/oxdock-process/src/env_policy.rs` (seed and reads) or
/// `crates/sys/oxdock-fs/src/env.rs` (plumbing).
pub fn render_env_contracts() -> Result<String> {
    use oxdock_process::env_policy;
    let mut out = String::from(env_policy::CONTRACT_INTRO_MD);
    out.push('\n');
    out.push_str(env_policy::BASELINE_INTRO_MD);
    out.push_str("\n| Variable | Platforms |\n| --- | --- |\n");
    for entry in env_policy::describe_baseline_defaults() {
        out.push_str(&format!(
            "| `{}` | {} |\n",
            escape_table_cell(entry.name),
            escape_table_cell(entry.platforms),
        ));
    }
    out.push('\n');
    out.push_str(env_policy::ROUTING_MD);
    out.push('\n');
    out.push_str(env_policy::HOST_READS_INTRO_MD);
    out.push_str("\n| Variable | Read by |\n| --- | --- |\n");
    for read in env_policy::engine_host_reads() {
        out.push_str(&format!(
            "| `{}` | {} |\n",
            escape_table_cell(read.name),
            escape_table_cell(read.reader),
        ));
    }
    out.push('\n');
    out.push_str(env_policy::PRECEDENCE_MD);
    out.push('\n');
    out.push_str(oxdock_fs::env::PLUMBING_INTRO_MD);
    out.push_str("\n| Variable | Owner | Rule |\n| --- | --- | --- |\n");
    for entry in oxdock_fs::env::plumbing() {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            escape_table_cell(entry.name),
            escape_table_cell(entry.owner),
            escape_table_cell(entry.rule)
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_renders_baseline_defaults() {
        // Every declared name renders in the baseline table with its
        // platforms; the table is mapped from the declaration, so this
        // pins the renderer against the contract.
        let text = render_env_contracts().expect("renders");
        assert!(
            text.contains("### Host-mirrored defaults"),
            "baseline section missing"
        );
        assert!(
            text.contains("| Variable | Platforms |"),
            "platforms column missing"
        );
        for entry in oxdock_process::env_policy::describe_baseline_defaults() {
            assert!(
                text.contains(&format!("`{}`", entry.name)),
                "{} missing",
                entry.name
            );
            assert!(
                text.contains(&format!("| `{}` | {} |", entry.name, entry.platforms)),
                "{} renders without its platforms",
                entry.name
            );
        }
        for name in [
            "SystemRoot",
            "PATHEXT",
            "TERM",
            "TMPDIR",
            "TEMP",
            "HOME",
            "LOGNAME",
            "USER",
            "PATH",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
        ] {
            assert!(text.contains(&format!("`{name}`")), "{name} missing");
        }
    }

    #[test]
    fn contract_renders_host_reads_from_core() {
        // Host interaction comes from core's engine_host_reads(), never
        // renderer prose: every declared read must appear with its reader.
        let text = render_env_contracts().expect("renders");
        assert!(
            text.contains("### Host reads"),
            "host reads section missing"
        );
        for read in oxdock_process::env_policy::engine_host_reads() {
            assert!(text.contains(read.reader), "reader {} missing", read.reader);
            assert!(
                text.contains(&format!("`{}`", read.name)),
                "read {} missing",
                read.name
            );
        }
        assert!(
            text.contains("CARGO_FEATURE_*"),
            "sanctioned seed pattern missing"
        );
    }

    #[test]
    fn contract_lists_every_plumbing_name() {
        // The plumbing table reads the macro-emitted ALL inventories:
        // every engine name renders, including EMBED_FORCE_REBUILD.
        let text = render_env_contracts().expect("renders");
        assert!(
            text.contains("### Engine plumbing"),
            "plumbing section missing"
        );
        for entry in oxdock_fs::env::plumbing() {
            assert!(
                text.contains(&format!("`{}`", entry.name)),
                "{} missing",
                entry.name
            );
        }
        for name in [
            "OXDOCK_EMBED_FORCE_REBUILD",
            "OXDOCK_WORKSPACE_ROOT",
            "CARGO_MANIFEST_DIR",
            "OUT_DIR",
        ] {
            assert!(text.contains(&format!("`{name}`")), "{name} missing");
        }
    }

    #[test]
    fn contract_chapters_come_from_core() {
        // Section bodies and the routing/enforcement chapters are core
        // constants: the renderer formats, never authors.
        let text = render_env_contracts().expect("renders");
        for chapter in [
            oxdock_process::env_policy::CONTRACT_INTRO_MD,
            oxdock_process::env_policy::BASELINE_INTRO_MD,
            oxdock_process::env_policy::ROUTING_MD,
        ] {
            assert!(
                text.contains(chapter.lines().next().unwrap_or_default()),
                "chapter missing: {}",
                chapter.lines().next().unwrap_or_default()
            );
        }
        assert!(
            text.contains(
                oxdock_process::env_policy::PRECEDENCE_MD
                    .lines()
                    .next()
                    .unwrap_or_default()
            ),
            "precedence chapter missing"
        );
    }
}
