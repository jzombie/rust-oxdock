use anyhow::Result;
use oxdock_parser::{Arg, StepKind, WorkspaceTarget};

/// Mock lowering for parser integration tests.
/// Uses MOCK_* names to verify grammar mechanics without domain coupling.
/// INHERIT_ENV is included as a real name because the parser validates it
/// post-parse by checking StepKind::InheritEnv — it's structural, not domain.
pub fn mock_lower(name: &str, args: Vec<Arg>) -> Result<StepKind> {
    match name {
        "MOCK_WRITE" => {
            let path = args.first().cloned().ok_or_else(|| anyhow::anyhow!("MOCK_WRITE requires path"))?;
            let contents = args.get(1).cloned();
            Ok(StepKind::Write { path, contents })
        }
        "MOCK_ENV" => {
            let arg = args.into_iter().next().ok_or_else(|| anyhow::anyhow!("MOCK_ENV requires key=val"))?;
            let (k, v) = arg.as_str().split_once('=').ok_or_else(|| anyhow::anyhow!("MOCK_ENV requires key=val"))?;
            Ok(StepKind::Env { key: k.to_string(), value: Arg::String(v.to_string()) })
        }
        "MOCK_ECHO" => {
            let msg = args.into_iter().next().ok_or_else(|| anyhow::anyhow!("MOCK_ECHO requires arg"))?;
            Ok(StepKind::Echo(msg))
        }
        "MOCK_RUN" => {
            let cmd = args.into_iter().next().ok_or_else(|| anyhow::anyhow!("MOCK_RUN requires arg"))?;
            Ok(StepKind::Run(cmd))
        }
        "MOCK_WORKDIR" => {
            let path = args.into_iter().next().ok_or_else(|| anyhow::anyhow!("MOCK_WORKDIR requires arg"))?;
            Ok(StepKind::Workdir(path))
        }
        "MOCK_WORKSPACE" => {
            let target = args.into_iter().next().ok_or_else(|| anyhow::anyhow!("MOCK_WORKSPACE requires target"))?;
            match target.as_str() {
                "SNAPSHOT" | "snapshot" => Ok(StepKind::Workspace(WorkspaceTarget::Snapshot)),
                "LOCAL" | "local" => Ok(StepKind::Workspace(WorkspaceTarget::Local)),
                _ => anyhow::bail!("unknown mock target"),
            }
        }
        _ => anyhow::bail!("unknown mock command: {name}"),
    }
}
