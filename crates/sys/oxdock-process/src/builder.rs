use std::{
    ffi::{OsStr, OsString},
    iter::IntoIterator,
};

#[cfg(miri)]
use anyhow::bail;
use anyhow::{Context, Result};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::fs::File;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::process::{Command as ProcessCommand, ExitStatus, Output as StdOutput, Stdio};

use crate::child::ChildHandle;

/// Builder wrapper that centralizes direct usages of `std::process::Command`.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct CommandBuilder {
    inner: ProcessCommand,
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl CommandBuilder {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let prog = program.as_ref().to_os_string();
        Self {
            inner: ProcessCommand::new(&prog),
            program: prog,
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        let val = arg.as_ref().to_os_string();
        self.inner.arg(&val);
        self.args.push(val);
        self
    }

    pub fn args<S, I>(&mut self, args: I) -> &mut Self
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_os_string();
        let value = value.as_ref().to_os_string();
        self.inner.env(&key, &value);
        self.envs.retain(|(k, _)| k != &key);
        self.envs.push((key, value));
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref();
        self.inner.env_remove(key);
        self.envs.retain(|(k, _)| k != key);
        self
    }

    pub fn stdin_file(&mut self, file: File) -> &mut Self {
        self.inner.stdin(Stdio::from(file));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        let path = dir.as_ref();
        self.inner.current_dir(path);
        self.cwd = Some(path.to_path_buf());
        self
    }

    pub fn status(&mut self) -> Result<ExitStatus> {
        #[cfg(miri)]
        {
            let snap = self.snapshot();
            synthetic_status(&snap)
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let status = self
                .inner
                .status()
                .with_context(|| format!("failed to run {desc}"))?;
            Ok(status)
        }
    }

    pub fn output(&mut self) -> Result<CommandOutput> {
        #[cfg(miri)]
        {
            let snap = self.snapshot();
            synthetic_output(&snap)
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let out = self
                .inner
                .output()
                .with_context(|| format!("failed to run {desc}"))?;
            Ok(CommandOutput::from(out))
        }
    }

    pub fn spawn(&mut self) -> Result<ChildHandle> {
        #[cfg(miri)]
        {
            bail!("spawn is not supported under miri synthetic process backend")
        }

        #[cfg(not(miri))]
        {
            let desc = format!("{:?}", self.inner);
            let child = self
                .inner
                .spawn()
                .with_context(|| format!("failed to spawn {desc}"))?;
            Ok(ChildHandle::new(child, Vec::new()))
        }
    }

    /// Return a lightweight snapshot of the command configuration for testing.
    pub fn snapshot(&self) -> CommandSnapshot {
        CommandSnapshot {
            program: self.program.clone(),
            args: self.args.clone(),
            envs: self.envs.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub struct CommandSnapshot {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub envs: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

#[allow(clippy::disallowed_types)]
impl From<StdOutput> for CommandOutput {
    fn from(value: StdOutput) -> Self {
        Self {
            status: value.status,
            stdout: value.stdout,
            stderr: value.stderr,
        }
    }
}

#[cfg(miri)]
use crate::synthetic::{synthetic_output, synthetic_status};

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_fs::GuardedPath;

    #[test]
    fn command_builder_snapshot_tracks_configuration() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let dir = temp.as_guarded_path().display().to_string();

        let mut builder = CommandBuilder::new("prog");
        builder.arg("a").args(["b", "c"]);
        builder.env("K", "V");
        builder.env("K", "V2"); // re-set replaces the earlier entry
        builder.env("GONE", "x");
        builder.env_remove("GONE");
        builder.current_dir(&dir);

        let snap = builder.snapshot();
        assert_eq!(snap.program, OsString::from("prog"));
        assert_eq!(
            snap.args,
            vec![
                OsString::from("a"),
                OsString::from("b"),
                OsString::from("c")
            ]
        );
        assert!(
            snap.envs
                .contains(&(OsString::from("K"), OsString::from("V2")))
        );
        assert!(
            !snap.envs.iter().any(|(k, _)| k == "GONE"),
            "env_remove must drop tracked entries"
        );
        assert_eq!(snap.cwd.as_deref(), Some(std::path::Path::new(&dir)));
    }
}
