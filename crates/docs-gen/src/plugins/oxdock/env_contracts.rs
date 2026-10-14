//! Live environment-variable contract reference: rendered from the
//! tier tables in `oxdock_process::env_policy`, so the documented
//! substrate can never drift from the assembled one. A parity test
//! pins that every assembled name is described and vice versa; this
//! renderer only formats.

use anyhow::Result;
use oxdock_markdown_plugin::markdown::escape_table_cell;

/// Render the full contract reference as markdown. Formatting only:
///
/// - tier chapters (title, body, rows) come from
///   `env_policy::tier_section` / `describe_tiers`,
/// - the enforcement chapter is `env_policy::PRECEDENCE_MD`,
/// - the host-read table is `env_policy::engine_host_reads`.
///
/// Nothing normative lives here; edits belong in
/// `crates/sys/oxdock-process/src/env_policy.rs`.
pub fn render_env_contracts() -> Result<String> {
    use oxdock_process::env_policy;
    let mut out = String::from(env_policy::CONTRACT_INTRO_MD);
    for tier in env_policy::tier_order() {
        let (_, body) = env_policy::tier_section(tier)
            .ok_or_else(|| anyhow::anyhow!("tier {tier} has rows but no section chapter"))?;
        let heading = env_policy::tier_heading(tier)
            .ok_or_else(|| anyhow::anyhow!("tier {tier} has rows but no heading"))?;
        out.push_str(&format!("\n{heading}\n\n{body}\n\n"));
        out.push_str("| Variable | Default | Rule |\n| --- | --- | --- |\n");
        for entry in env_policy::describe_tiers()
            .iter()
            .filter(|entry| entry.tier == tier)
        {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                escape_table_cell(entry.name),
                escape_table_cell(entry.default),
                escape_table_cell(entry.rule)
            ));
        }
    }
    out.push_str(env_policy::HOST_READS_INTRO_MD);
    out.push_str("\n| Variable | Read by | Purpose |\n| --- | --- | --- |\n");
    for read in env_policy::engine_host_reads() {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            escape_table_cell(read.name),
            escape_table_cell(read.reader),
            escape_table_cell(read.purpose)
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
    fn contract_renders_every_tier() {
        // Tier labels come from core's tier_order(): a new tier renders
        // without touching this test.
        let text = render_env_contracts().expect("renders");
        for tier in oxdock_process::env_policy::tier_order() {
            let heading = oxdock_process::env_policy::tier_heading(tier)
                .expect("core headings cover every tier");
            assert!(text.contains(&heading), "tier {tier} section missing");
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
        ] {
            assert!(text.contains(&format!("`{name}`")), "{name} missing");
        }
    }

    #[test]
    fn contract_names_match_live_consts() {
        // The table must name the exact constant values, so a renamed
        // constant fails here instead of drifting the doc.
        use oxdock_fs::env::os::{unix, windows};
        let text = render_env_contracts().expect("renders");
        for name in [
            windows::substrate::SYSTEM_ROOT,
            windows::substrate::WINDIR,
            windows::substrate::PATHEXT,
            unix::substrate::TERM,
            windows::managed::TEMP,
            windows::managed::TMP,
            unix::managed::TMPDIR,
            unix::managed::HOME,
            unix::LOGNAME,
            unix::USER,
            windows::USERNAME,
            windows::USERPROFILE,
            oxdock_fs::env::os::PATH,
        ] {
            assert!(text.contains(name), "{name} missing");
        }
    }

    #[test]
    fn contract_renders_host_reads_from_core() {
        // Host interaction comes from core's engine_host_reads(), never
        // renderer prose: every declared read must appear.
        let text = render_env_contracts().expect("renders");
        assert!(
            text.contains("### Host reads"),
            "host reads section missing"
        );
        for read in oxdock_process::env_policy::engine_host_reads() {
            assert!(text.contains(read.reader), "reader {} missing", read.reader);
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
        // Section bodies and the enforcement chapter are core constants:
        // the renderer formats, never authors.
        let text = render_env_contracts().expect("renders");
        for tier in oxdock_process::env_policy::tier_order() {
            let (_, body) = oxdock_process::env_policy::tier_section(tier)
                .expect("core chapters cover every tier");
            assert!(
                text.contains(body.lines().next().unwrap_or_default()),
                "tier {tier} body missing"
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
