use anyhow::{Result, anyhow, bail};
use oxdock_parser::{
    Arg, ArgSpec, CommandMeta, CommandSpec, Example, FlagSpec, FlagValueType, IoDirection,
    StepKind, Stream, WorkspaceTarget, strip_flags,
};

/// Join multiple `Arg` tokens into a single `Arg::String` with spaces.
/// Used by commands that accept a single string payload from multiple
/// unquoted tokens (e.g. `RUN cargo build` → `"cargo build"`).
fn join_args(args: Vec<Arg>, cmd_name: &str) -> Result<Arg> {
    if args.is_empty() {
        bail!("{cmd_name} requires at least one argument");
    }
    if args.len() == 1 {
        return Ok(args.into_iter().next().unwrap());
    }
    let joined: String = args
        .iter()
        .map(|a| a.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Arg::String(joined))
}

macro_rules! arg {
    ($name:expr, $type:expr, $io:expr, $idx:expr, $req:expr) => {
        ArgSpec {
            name: $name,
            arg_type: $type,
            io: $io,
            index: $idx,
            required: $req,
            fallback_stream: None,
        }
    };
    ($name:expr, $type:expr, $io:expr, $idx:expr, $req:expr, $stream:expr) => {
        ArgSpec {
            name: $name,
            arg_type: $type,
            io: $io,
            index: $idx,
            required: $req,
            fallback_stream: $stream,
        }
    };
}

macro_rules! cmd_meta {
    ($name:expr, $syntax:expr, $summary:expr, $desc:expr, $args:expr, $flags:expr, $out:expr, $examples:expr) => {
        CommandMeta {
            name: $name,
            syntax: $syntax,
            summary: $summary,
            description: $desc,
            args: $args,
            flags: $flags,
            default_output: $out,
            examples: $examples,
        }
    };
}

// ── WORKDIR ─────────────────────────────────────────────────────────────────

pub struct WorkdirCmd;

impl CommandSpec for WorkdirCmd {
    const NAME: &'static str = "WORKDIR";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "WORKDIR",
            "WORKDIR <path>",
            "Change the working directory for subsequent steps.",
            "Sets the current working directory. Relative paths resolve against the current root.",
            &[arg!("path", "string", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[Example { name: "change dir", code: "WORKDIR src" }]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let path = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("WORKDIR requires a path"))?;
        Ok(StepKind::Workdir(path))
    }
}

// ── WORKSPACE ───────────────────────────────────────────────────────────────

pub struct WorkspaceCmd;

impl CommandSpec for WorkspaceCmd {
    const NAME: &'static str = "WORKSPACE";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "WORKSPACE",
            "WORKSPACE SNAPSHOT|LOCAL",
            "Switch between snapshot and local workspace roots.",
            "SNAPSHOT targets the read-only snapshot root; LOCAL targets the mutable build-context root.",
            &[arg!("target", "SNAPSHOT|LOCAL", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let target = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("WORKSPACE requires a target"))?;
        let wt = match target.as_str() {
            "SNAPSHOT" | "snapshot" => WorkspaceTarget::Snapshot,
            "LOCAL" | "local" => WorkspaceTarget::Local,
            _ => bail!("unknown workspace target: {}", target.as_str()),
        };
        Ok(StepKind::Workspace(wt))
    }
}

// ── ENV ─────────────────────────────────────────────────────────────────────

pub struct EnvCmd;

impl CommandSpec for EnvCmd {
    const NAME: &'static str = "ENV";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ENV",
            "ENV KEY=value",
            "Set an environment variable for subsequent steps.",
            "Inserts or updates an environment variable. The value is an expandable string.",
            &[arg!("assignment", "KEY=value", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[Example { name: "set env", code: "ENV FOO=bar" }]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let arg = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("ENV requires KEY=value"))?;
        let raw = arg.as_str();
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("ENV requires KEY=value format"))?;
        let value = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
        Ok(StepKind::Env {
            key: k.to_string(),
            value: Arg::String(value.to_string()),
        })
    }
}

// ── ECHO ────────────────────────────────────────────────────────────────────

pub struct EchoCmd;

impl CommandSpec for EchoCmd {
    const NAME: &'static str = "ECHO";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ECHO",
            "ECHO <message>",
            "Print a message to stdout.",
            "Outputs the message to stdout. Supports template expansion.",
            &[arg!("message", "string", IoDirection::Write, 0, true, None)],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let msg = join_args(args, "ECHO")?;
        Ok(StepKind::Echo(msg))
    }
}

// ── RUN ─────────────────────────────────────────────────────────────────────

pub struct RunCmd;

impl CommandSpec for RunCmd {
    const NAME: &'static str = "RUN";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "RUN",
            "RUN <command...>",
            "Execute a shell command.",
            "Runs the command in the current working directory. Arguments are joined with spaces.",
            &[arg!("command", "string...", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[Example { name: "run cargo", code: "RUN cargo build" }]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let cmd = join_args(args, "RUN")?;
        Ok(StepKind::Run(cmd))
    }
}

// ── RUN_BG ──────────────────────────────────────────────────────────────────

pub struct RunBgCmd;

impl CommandSpec for RunBgCmd {
    const NAME: &'static str = "RUN_BG";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "RUN_BG",
            "RUN_BG <command...>",
            "Execute a shell command in the background.",
            "Spawns the command without blocking. The pipeline terminates background processes on exit.",
            &[arg!("command", "string...", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let cmd = join_args(args, "RUN_BG")?;
        Ok(StepKind::RunBg(cmd))
    }
}

// ── COPY ────────────────────────────────────────────────────────────────────

pub struct CopyCmd;

impl CommandSpec for CopyCmd {
    const NAME: &'static str = "COPY";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "COPY",
            "COPY [--from-current-workspace] <from> <to>",
            "Copy a file or directory into the workspace.",
            "Copies a file/directory from the host filesystem into the workspace. Use --from-current-workspace to copy from the active workspace root instead of the build context.",
            &[
                arg!("from", "path", IoDirection::Read, 0, true, None),
                arg!("to", "path", IoDirection::Write, 1, true, None),
            ],
            &[FlagSpec {
                name: "from_current_workspace",
                long: "--from-current-workspace",
                value_type: FlagValueType::Flag,
                required: false,
                description: "Copy from the current workspace root instead of build context",
            }],
            None,
            &[]
        )
    }

    fn lower(flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let from_current_workspace = flags.iter().any(|(k, _)| k == "from_current_workspace");
        let from = args
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("COPY requires a source path"))?;
        let to = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow!("COPY requires a destination path"))?;
        Ok(StepKind::Copy {
            from_current_workspace,
            from,
            to,
        })
    }
}

// ── COPY_GIT ────────────────────────────────────────────────────────────────

pub struct CopyGitCmd;

impl CommandSpec for CopyGitCmd {
    const NAME: &'static str = "COPY_GIT";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "COPY_GIT",
            "COPY_GIT [--include-dirty] <rev> <src> <dst>",
            "Copy a file or directory from a git revision.",
            "Checks out a specific git revision and copies the specified path into the workspace.",
            &[
                arg!("rev", "string", IoDirection::Read, 0, true, None),
                arg!("src", "path", IoDirection::Read, 1, true, None),
                arg!("dst", "path", IoDirection::Write, 2, true, None),
            ],
            &[FlagSpec {
                name: "dirty",
                long: "--include-dirty",
                value_type: FlagValueType::Flag,
                required: false,
                description: "Include uncommitted changes",
            }],
            None,
            &[]
        )
    }

    fn lower(flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let include_dirty = flags.iter().any(|(k, _)| k == "dirty");
        let rev = args
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("COPY_GIT requires a revision"))?;
        let from = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow!("COPY_GIT requires a source path"))?;
        let to = args
            .get(2)
            .cloned()
            .ok_or_else(|| anyhow!("COPY_GIT requires a destination path"))?;
        Ok(StepKind::CopyGit {
            rev,
            from,
            to,
            include_dirty,
        })
    }
}

// ── SYMLINK ─────────────────────────────────────────────────────────────────

pub struct SymlinkCmd;

impl CommandSpec for SymlinkCmd {
    const NAME: &'static str = "SYMLINK";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "SYMLINK",
            "SYMLINK <from> <to>",
            "Create a symbolic link.",
            "Creates a symlink at 'to' pointing to 'from'.",
            &[
                arg!("from", "path", IoDirection::Read, 0, true, None),
                arg!("to", "path", IoDirection::Write, 1, true, None),
            ],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let from = args
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("SYMLINK requires a source"))?;
        let to = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow!("SYMLINK requires a target"))?;
        Ok(StepKind::Symlink { from, to })
    }
}

// ── MKDIR ───────────────────────────────────────────────────────────────────

pub struct MkdirCmd;

impl CommandSpec for MkdirCmd {
    const NAME: &'static str = "MKDIR";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "MKDIR",
            "MKDIR <path>",
            "Create a directory.",
            "Creates the directory at the given path, including parents.",
            &[arg!("path", "path", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let path = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("MKDIR requires a path"))?;
        Ok(StepKind::Mkdir(path))
    }
}

// ── LS ──────────────────────────────────────────────────────────────────────

pub struct LsCmd;

impl CommandSpec for LsCmd {
    const NAME: &'static str = "LS";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "LS",
            "LS [<path>]",
            "List directory contents.",
            "Lists entries in the given directory, or the current directory if omitted.",
            &[arg!("path", "path", IoDirection::Read, 0, false, None)],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        Ok(StepKind::Ls(args.into_iter().next()))
    }
}

// ── CWD ─────────────────────────────────────────────────────────────────────

pub struct CwdCmd;

impl CommandSpec for CwdCmd {
    const NAME: &'static str = "CWD";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "CWD",
            "CWD",
            "Print the current working directory.",
            "Outputs the current working directory to stdout.",
            &[],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, _args: Vec<Arg>) -> Result<StepKind> {
        Ok(StepKind::Cwd)
    }
}

// ── READ ────────────────────────────────────────────────────────────────────

pub struct ReadCmd;

impl CommandSpec for ReadCmd {
    const NAME: &'static str = "READ";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "READ",
            "READ [<path>]",
            "Read file contents to stdout.",
            "Outputs the file contents. If no path, reads from stdin.",
            &[arg!("path", "path", IoDirection::Read, 0, false, None)],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        Ok(StepKind::Read(args.into_iter().next()))
    }
}

// ── WRITE ───────────────────────────────────────────────────────────────────

pub struct WriteCmd;

impl CommandSpec for WriteCmd {
    const NAME: &'static str = "WRITE";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "WRITE",
            "WRITE <path> [<contents>]",
            "Write contents to a file.",
            "Writes the contents to the specified file, creating or overwriting it.",
            &[
                arg!("path", "path", IoDirection::Write, 0, true, None),
                arg!("contents", "string", IoDirection::Write, 1, false, Some(Stream::Stdin)),
            ],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, mut args: Vec<Arg>) -> Result<StepKind> {
        if args.is_empty() {
            bail!("WRITE requires a path");
        }
        let path = args.remove(0);
        let contents = if args.is_empty() {
            None
        } else {
            Some(join_args(args, "WRITE")?)
        };
        Ok(StepKind::Write { path, contents })
    }
}

// ── APPEND ──────────────────────────────────────────────────────────────────

pub struct AppendCmd;

impl CommandSpec for AppendCmd {
    const NAME: &'static str = "APPEND";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "APPEND",
            "APPEND <path> [<contents>]",
            "Append contents to a file.",
            "Appends the contents to the specified file, creating it if it doesn't exist.",
            &[
                arg!("path", "path", IoDirection::Write, 0, true, None),
                arg!("contents", "string", IoDirection::Write, 1, false, Some(Stream::Stdin)),
            ],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, mut args: Vec<Arg>) -> Result<StepKind> {
        if args.is_empty() {
            bail!("APPEND requires a path");
        }
        let path = args.remove(0);
        let contents = if args.is_empty() {
            None
        } else {
            Some(join_args(args, "APPEND")?)
        };
        Ok(StepKind::Append { path, contents })
    }
}

// ── EXPAND ──────────────────────────────────────────────────────────────────

pub struct ExpandCmd;

impl CommandSpec for ExpandCmd {
    const NAME: &'static str = "EXPAND";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "EXPAND",
            "EXPAND [<path>] [<KEY=val> ...]",
            "Expand template placeholders in a file.",
            "Reads the file (or stdin), expands {{ env:KEY }} placeholders, and outputs to stdout.",
            &[
                arg!("path", "path", IoDirection::Read, 0, false, None),
            ],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let mut path = None;
        let mut overrides = Vec::new();
        for arg in args {
            let s = arg.as_str();
            if let Some((k, v)) = s.split_once('=') {
                overrides.push((k.to_string(), Arg::String(v.to_string())));
            } else if path.is_none() {
                path = Some(arg);
            } else {
                bail!("EXPAND accepts at most one path argument");
            }
        }
        Ok(StepKind::Expand { path, overrides })
    }
}

// ── ASSERT_FILE ─────────────────────────────────────────────────────────────

pub struct AssertFileCmd;

impl CommandSpec for AssertFileCmd {
    const NAME: &'static str = "ASSERT_FILE";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ASSERT_FILE",
            "ASSERT_FILE [--hash <sha256>] <path> [<expected>]",
            "Assert a file exists and optionally matches expected content.",
            "Verifies the file exists. With --hash, checks the SHA-256 digest. With an expected argument, checks the contents match.",
            &[
                arg!("path", "path", IoDirection::Read, 0, true, None),
                arg!("expected", "string", IoDirection::Read, 1, false, None),
            ],
            &[FlagSpec {
                name: "hash",
                long: "--hash",
                value_type: FlagValueType::String,
                required: false,
                description: "Expected SHA-256 hash of the file contents",
            }],
            None,
            &[]
        )
    }

    fn lower(flags: Vec<(String, Arg)>, mut args: Vec<Arg>) -> Result<StepKind> {
        let hash = flags
            .iter()
            .find(|(k, _)| k == "hash")
            .map(|(_, v)| v.as_str().to_string());
        if args.is_empty() {
            bail!("ASSERT_FILE requires a path");
        }
        let path = args.remove(0);
        let contents = if args.is_empty() {
            None
        } else {
            Some(join_args(args, "ASSERT_FILE")?)
        };
        Ok(StepKind::AssertFile {
            hash,
            path,
            contents,
        })
    }
}

// ── ASSERT_DIR ──────────────────────────────────────────────────────────────

pub struct AssertDirCmd;

impl CommandSpec for AssertDirCmd {
    const NAME: &'static str = "ASSERT_DIR";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ASSERT_DIR",
            "ASSERT_DIR <path>",
            "Assert a directory exists.",
            "Verifies the directory exists.",
            &[arg!("path", "path", IoDirection::Read, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let path = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("ASSERT_DIR requires a path"))?;
        Ok(StepKind::AssertDir(path))
    }
}

// ── ASSERT_ABSENT ───────────────────────────────────────────────────────────

pub struct AssertAbsentCmd;

impl CommandSpec for AssertAbsentCmd {
    const NAME: &'static str = "ASSERT_ABSENT";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ASSERT_ABSENT",
            "ASSERT_ABSENT <path>",
            "Assert a file or directory does not exist.",
            "Verifies the path does not exist.",
            &[arg!("path", "path", IoDirection::Read, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let path = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("ASSERT_ABSENT requires a path"))?;
        Ok(StepKind::AssertAbsent(path))
    }
}

// ── ASSERT_STDOUT ───────────────────────────────────────────────────────────

pub struct AssertStdoutCmd;

impl CommandSpec for AssertStdoutCmd {
    const NAME: &'static str = "ASSERT_STDOUT";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "ASSERT_STDOUT",
            "ASSERT_STDOUT <substring>",
            "Assert stdout contains a substring.",
            "Verifies that subsequent command output contains the given substring.",
            &[arg!("substring", "string", IoDirection::Read, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let needle = join_args(args, "ASSERT_STDOUT")?;
        Ok(StepKind::AssertStdout(needle))
    }
}

// ── HASH_SHA256 ─────────────────────────────────────────────────────────────

pub struct HashSha256Cmd;

impl CommandSpec for HashSha256Cmd {
    const NAME: &'static str = "HASH_SHA256";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "HASH_SHA256",
            "HASH_SHA256 <path>",
            "Print the SHA-256 hash of a file.",
            "Computes and outputs the SHA-256 digest of the file contents.",
            &[arg!("path", "path", IoDirection::Read, 0, true, None)],
            &[],
            Some(Stream::Stdout),
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let path = args
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("HASH_SHA256 requires a path"))?;
        Ok(StepKind::HashSha256 { path })
    }
}

// ── EXIT ────────────────────────────────────────────────────────────────────

pub struct ExitCmd;

impl CommandSpec for ExitCmd {
    const NAME: &'static str = "EXIT";

    fn metadata() -> CommandMeta {
        cmd_meta!(
            "EXIT",
            "EXIT <code>",
            "Exit the pipeline with a status code.",
            "Terminates the pipeline immediately with the given exit code.",
            &[arg!("code", "int", IoDirection::Write, 0, true, None)],
            &[],
            None,
            &[]
        )
    }

    fn lower(_flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind> {
        let code = args
            .into_iter()
            .next()
            .and_then(|a| a.as_str().parse::<i32>().ok())
            .unwrap_or(0);
        Ok(StepKind::Exit(code))
    }
}

// ── lower_command ───────────────────────────────────────────────────────────

/// Lower a command by name: strip flags, then call `CommandSpec::lower`.
pub fn lower_command(name: &str, raw_args: Vec<Arg>) -> Result<StepKind> {
    match name {
        "WORKDIR" => dispatch::<WorkdirCmd>(raw_args),
        "WORKSPACE" => dispatch::<WorkspaceCmd>(raw_args),
        "ENV" => dispatch::<EnvCmd>(raw_args),
        "ECHO" => dispatch::<EchoCmd>(raw_args),
        "RUN" => dispatch::<RunCmd>(raw_args),
        "RUN_BG" => dispatch::<RunBgCmd>(raw_args),
        "COPY" => dispatch::<CopyCmd>(raw_args),
        "COPY_GIT" => dispatch::<CopyGitCmd>(raw_args),
        "SYMLINK" => dispatch::<SymlinkCmd>(raw_args),
        "MKDIR" => dispatch::<MkdirCmd>(raw_args),
        "LS" => dispatch::<LsCmd>(raw_args),
        "CWD" => dispatch::<CwdCmd>(raw_args),
        "READ" => dispatch::<ReadCmd>(raw_args),
        "WRITE" => dispatch::<WriteCmd>(raw_args),
        "APPEND" => dispatch::<AppendCmd>(raw_args),
        "EXPAND" => dispatch::<ExpandCmd>(raw_args),
        "ASSERT_FILE" => dispatch::<AssertFileCmd>(raw_args),
        "ASSERT_DIR" => dispatch::<AssertDirCmd>(raw_args),
        "ASSERT_ABSENT" => dispatch::<AssertAbsentCmd>(raw_args),
        "ASSERT_STDOUT" => dispatch::<AssertStdoutCmd>(raw_args),
        "HASH_SHA256" => dispatch::<HashSha256Cmd>(raw_args),
        "EXIT" => dispatch::<ExitCmd>(raw_args),
        _ => bail!("unknown command: {name}"),
    }
}

fn dispatch<C: CommandSpec>(raw_args: Vec<Arg>) -> Result<StepKind> {
    let meta = C::metadata();
    let (flags, positional) = strip_flags(raw_args, &meta)?;
    C::lower(flags, positional)
}
