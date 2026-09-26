//! Single-flag remote target registration: `--remote <target>="<command>"`.
//!
//! One flag binds one inventory target to one transport command string.
//! The string is tokenized ONCE at startup with POSIX shell-word rules:
//! naive whitespace splitting corrupts quoted SSH options (for example
//! `-o "ProxyCommand=ssh -W %h:%p jump"`). DSL scripts keep target-only
//! identifiers; every transport byte stays in the CLI invocation.

use anyhow::{Result, bail};

/// Parse one `--remote` flag value of the form `<target>=<command>`.
/// Returns the target name plus the raw command string; tokenization
/// happens at inventory build so quoting errors fail fast with the flag
/// text attached.
pub fn parse_remote_arg(raw: &str) -> Result<(String, String)> {
    let Some((target, command)) = raw.split_once('=') else {
        bail!(
            "invalid --remote {raw:?}: expected <target>=\"<command>\", e.g. --remote prod=\"ssh user@host oxdock\""
        );
    };
    let target = target.trim();
    if !oxdock_parser::commands::is_valid_remote_target(target) {
        bail!(
            "invalid --remote target {target:?}: start alphanumeric, then alphanumeric, `_`, or `-`, max 64 chars, never SNAPSHOT, LOCAL, CACHE, SYSTEM, or REMOTE"
        );
    }
    if command.trim().is_empty() {
        bail!("invalid --remote {target:?}: command string is empty");
    }
    if let Err(err) = shell_words::split(command) {
        bail!("invalid --remote {target:?}: cannot tokenize command: {err}");
    }
    Ok((target.to_string(), command.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_target_command_shapes() {
        let (target, command) = parse_remote_arg("prod=\"ssh user@host oxdock\"").unwrap();
        assert_eq!(target, "prod");
        assert_eq!(command, "\"ssh user@host oxdock\"");
        let (target, _) = parse_remote_arg("win-arm=ssh -i key Administrator@host C:/oxdock.exe").unwrap();
        assert_eq!(target, "win-arm");
    }

    #[test]
    fn quoted_ssh_options_survive() {
        let (_, command) =
            parse_remote_arg("prod=\"ssh -o \\\"ProxyCommand=ssh -W %h:%p jump\\\" user@host oxdock\"")
                .unwrap();
        let argv = shell_words::split(&command).unwrap();
        assert!(argv.iter().any(|arg| arg.contains("ProxyCommand")));
    }

    #[test]
    fn malformed_flags_bail() {
        for bad in [
            "no-equals-sign",
            "=ssh host oxdock",
            "LOCAL=ssh host oxdock",
            "REMOTE=ssh host oxdock",
            "-lead=ssh host oxdock",
            "prod=",
            "prod=   ",
            "prod=\"ssh user@host",
        ] {
            assert!(parse_remote_arg(bad).is_err(), "{bad:?} must fail");
        }
    }
}
