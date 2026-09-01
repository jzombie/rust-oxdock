use anyhow::{Result, anyhow, bail};
use indoc::indoc;
use oxdock_parser::{
    Arg, ArgSpec, CommandMeta, CommandSpec, Example, FlagSpec, FlagValueType, IoDirection,
    StepKind, Stream, WorkspaceTarget,
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
    ($name:expr, $type:expr, $desc:expr, $io:expr, $idx:expr, $req:expr) => {
        ArgSpec {
            name: $name,
            arg_type: $type,
            description: $desc,
            io: $io,
            index: $idx,
            required: $req,
            fallback_stream: None,
        }
    };
    ($name:expr, $type:expr, $desc:expr, $io:expr, $idx:expr, $req:expr, $stream:expr) => {
        ArgSpec {
            name: $name,
            arg_type: $type,
            description: $desc,
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
            &[arg!(
                "path",
                "string",
                "Directory to change to",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "change working directory",
                fence_meta: None,
                code: indoc! {r#"
                    // Relative WORKDIR; later relative paths resolve against it.
                    WORKDIR project/src
                    WRITE generated.txt generated-under-workdir
                    ASSERT_FILE generated.txt generated-under-workdir
                "#},
            }]
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
            &[arg!(
                "target",
                "SNAPSHOT|LOCAL",
                "SNAPSHOT or LOCAL",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "switch workspace roots",
                fence_meta: None,
                code: indoc! {r#"
                    // Write to isolated SNAPSHOT root
                    WRITE workspace-note.txt "written-into-workspace"
                    ASSERT_FILE workspace-note.txt "written-into-workspace"

                    // LOCAL root does not contain SNAPSHOT files
                    WORKSPACE LOCAL
                    ASSERT_ABSENT workspace-note.txt

                    // Return to default root
                    WORKSPACE SNAPSHOT
                "#},
            }]
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
            &[arg!(
                "assignment",
                "KEY=value",
                "KEY=value pair",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "set and read environment variable",
                fence_meta: None,
                code: indoc! {r#"
                    ENV APP_MODE=production

                    // Guards read script variables set by ENV.
                    [env:APP_MODE==production] ECHO running-in-production
                    ASSERT_STDOUT running-in-production
                "#},
            }]
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
        let value = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(v);
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
            &[arg!(
                "message",
                "string",
                "Text to print",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            Some(Stream::Stdout),
            &[Example {
                name: "print message with assertion",
                fence_meta: None,
                code: indoc! {r#"
                    ECHO build-complete
                    ASSERT_STDOUT build-complete
                "#},
            }]
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
            "Runs the command in the current working directory. Arguments are joined with spaces. Child stdout/stderr stream to the script's configured outputs, and a non-zero exit code fails the script. This is the one intentionally platform-specific command: use platform guards to provide per-OS invocations when needed.",
            &[arg!(
                "command",
                "string...",
                "Shell command to execute",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "run platform-specific command",
                fence_meta: None,
                code: indoc! {r#"
                    // Host shell differs per OS: pick the invocation with guards.
                    [unix] RUN echo native-unix-shell
                    [windows] RUN cmd /c echo native-windows-shell

                    // Child output streams into the script's stdout.
                    [unix] ASSERT_STDOUT native-unix-shell
                    [windows] ASSERT_STDOUT native-windows-shell
                "#},
            }]
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
            "Like RUN, but spawns the command in the background and continues the script. If any background child finishes early with a non-zero status, the script fails and remaining children are killed. At script end the first child is awaited to completion and the remainder are killed. Background children started before an EXIT are killed before unwinding.",
            &[arg!(
                "command",
                "string...",
                "Shell command to run in background",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "background process lifecycle",
                fence_meta: None,
                code: indoc! {r#"
                    // Spawn a slow child; the script does NOT wait for it here.
                    [unix] RUN_BG sleep 1
                    [windows] RUN_BG ping -n 2 127.0.0.1

                    // Mainline continues immediately.
                    ECHO mainline-continues-immediately
                    ASSERT_STDOUT mainline-continues-immediately
                "#},
            }]
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
            "Copies a file/directory from the host filesystem into the workspace. Plain COPY <from> <to> resolves <from> against the build context (the tree OxDock was invoked against), regardless of the current working directory. COPY --from-current-workspace <from> <to> resolves <from> against the workspace root instead. Parent directories at the destination are created on demand.",
            &[
                arg!(
                    "from",
                    "path",
                    "Source path (build context or workspace root)",
                    IoDirection::Read,
                    0,
                    true,
                    None
                ),
                arg!(
                    "to",
                    "path",
                    "Destination path in workspace",
                    IoDirection::Write,
                    1,
                    true,
                    None
                ),
            ],
            &[FlagSpec {
                name: "from_current_workspace",
                long: "--from-current-workspace",
                value_type: FlagValueType::Flag,
                required: false,
                description: "Copy from the current workspace root instead of build context",
            }],
            None,
            &[Example {
                name: "copy with default resolution",
                fence_meta: Some("roots:unified"),
                code: indoc! {r#"
                    // Seed a file at the (unified) build-context root.
                    WRITE context-file.txt copied-by-default-resolution
                    MKDIR app

                    // Default form: source resolves against the build context.
                    COPY context-file.txt app/local-copy.txt
                    ASSERT_FILE app/local-copy.txt copied-by-default-resolution
                "#},
            }]
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
                arg!(
                    "rev",
                    "string",
                    "Git revision spec",
                    IoDirection::Read,
                    0,
                    true,
                    None
                ),
                arg!(
                    "src",
                    "path",
                    "Source path in repository",
                    IoDirection::Read,
                    1,
                    true,
                    None
                ),
                arg!(
                    "dst",
                    "path",
                    "Destination path in workspace",
                    IoDirection::Write,
                    2,
                    true,
                    None
                ),
            ],
            &[FlagSpec {
                name: "dirty",
                long: "--include-dirty",
                value_type: FlagValueType::Flag,
                required: false,
                description: "Include uncommitted changes",
            }],
            None,
            &[Example {
                name: "copy from git revision",
                fence_meta: Some("roots:unified"),
                code: indoc! {r#"
                    // Initialize a temporary repository.
                    RUN git init -q .
                    WRITE tracked.txt committed-content
                    RUN git add tracked.txt
                    RUN git -c user.name=oxdock-docs -c user.email=docs@oxdock.invalid commit -qm init

                    // Recover the committed blob from history into the workspace.
                    COPY_GIT HEAD tracked.txt restored.txt
                    ASSERT_FILE restored.txt committed-content
                "#},
            }]
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
                arg!(
                    "from",
                    "path",
                    "Target of the symlink",
                    IoDirection::Read,
                    0,
                    true,
                    None
                ),
                arg!(
                    "to",
                    "path",
                    "Link path to create",
                    IoDirection::Write,
                    1,
                    true,
                    None
                ),
            ],
            &[],
            None,
            &[Example {
                name: "create symbolic link",
                fence_meta: Some("roots:unified"),
                code: indoc! {r#"
                    WRITE original.txt linked-content

                    // link.txt references original.txt (or copies on Windows).
                    SYMLINK original.txt link.txt
                    READ link.txt
                    ASSERT_STDOUT linked-content
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "Directory path to create",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "create nested directories",
                fence_meta: None,
                code: indoc! {r#"
                    // Creates every missing parent.
                    MKDIR deeply/nested/tree
                    ASSERT_DIR deeply/nested/tree
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "Directory to list (optional)",
                IoDirection::Read,
                0,
                false,
                None
            )],
            &[],
            Some(Stream::Stdout),
            &[Example {
                name: "list directory contents",
                fence_meta: None,
                code: indoc! {r#"
                    MKDIR inventory
                    WRITE inventory/alpha.txt first
                    WRITE inventory/beta.txt second

                    // Prints entries sorted by name.
                    LS inventory
                    ASSERT_STDOUT alpha.txt
                    ASSERT_STDOUT beta.txt
                "#},
            }]
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
            &[Example {
                name: "print current directory",
                fence_meta: None,
                code: indoc! {r#"
                    WORKDIR level-one/level-two

                    // Prints the canonical physical path.
                    CWD
                    ASSERT_STDOUT level-two
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "File to read (optional, stdin if omitted)",
                IoDirection::Read,
                0,
                false,
                None
            )],
            &[],
            Some(Stream::Stdout),
            &[Example {
                name: "read file to stdout",
                fence_meta: None,
                code: indoc! {r#"
                    WRITE note.txt file-read-back

                    // Raw bytes in, raw bytes out.
                    READ note.txt
                    ASSERT_STDOUT file-read-back
                "#},
            }]
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
            "Writes contents to a file, replacing any existing contents. Creates parent directories on demand. Without a contents argument it consumes the script's stdin instead (combine with WITH_IO [stdin...]).",
            &[
                arg!(
                    "path",
                    "path",
                    "File path to write",
                    IoDirection::Write,
                    0,
                    true,
                    None
                ),
                arg!(
                    "contents",
                    "string",
                    "File contents (optional, stdin if omitted)",
                    IoDirection::Write,
                    1,
                    false,
                    Some(Stream::Stdin)
                ),
            ],
            &[],
            None,
            &[Example {
                name: "write and verify file",
                fence_meta: None,
                code: indoc! {r#"
                    WRITE output.txt hello-world
                    ASSERT_FILE output.txt hello-world

                    // Pipe READ output into WRITE via WITH_IO.
                    WRITE input.txt captured-body
                    WITH_IO [stdout=pipe:data] READ input.txt
                    WITH_IO [stdin=pipe:data] WRITE captured.txt
                    ASSERT_FILE captured.txt captured-body
                "#},
            }]
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
                arg!(
                    "path",
                    "path",
                    "File path to append to",
                    IoDirection::Write,
                    0,
                    true,
                    None
                ),
                arg!(
                    "contents",
                    "string",
                    "Content to append (optional, stdin if omitted)",
                    IoDirection::Write,
                    1,
                    false,
                    Some(Stream::Stdin)
                ),
            ],
            &[],
            None,
            &[Example {
                name: "append to file",
                fence_meta: None,
                code: indoc! {r#"
                    WRITE log.txt line1
                    APPEND log.txt line2
                    ASSERT_FILE log.txt line1line2
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "Template file path (optional, stdin if omitted)",
                IoDirection::Read,
                0,
                false,
                None
            ),],
            &[],
            Some(Stream::Stdout),
            &[Example {
                name: "expand template file",
                fence_meta: None,
                code: indoc! {r#"
                    // Create a template file with literal {{ env:KEY }} tags.
                    WRITE template.md "Hello, \{{ env:NAME }}!"

                    // Expand the template with an explicit override.
                    EXPAND template.md NAME="Alice"
                    ASSERT_STDOUT "Hello, Alice!"
                "#},
            }]
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
                arg!(
                    "path",
                    "path",
                    "File path to verify",
                    IoDirection::Read,
                    0,
                    true,
                    None
                ),
                arg!(
                    "expected",
                    "string",
                    "Expected file contents",
                    IoDirection::Read,
                    1,
                    false,
                    None
                ),
            ],
            &[FlagSpec {
                name: "hash",
                long: "--hash",
                value_type: FlagValueType::String,
                required: false,
                description: "Expected SHA-256 hash of the file contents",
            }],
            None,
            &[Example {
                name: "verify file contents",
                fence_meta: None,
                code: indoc! {r#"
                    WRITE payload.bin stable-content

                    // Exact-byte comparison.
                    ASSERT_FILE payload.bin stable-content

                    // Digest comparison for trailing newlines or binary bytes.
                    ASSERT_FILE --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c payload.bin
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "Directory path to verify",
                IoDirection::Read,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "verify directory exists",
                fence_meta: None,
                code: indoc! {r#"
                    MKDIR dist/assets
                    ASSERT_DIR dist/assets
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "Path that must not exist",
                IoDirection::Read,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "verify path does not exist",
                fence_meta: None,
                code: indoc! {r#"
                    // Neither variable exists, so this guard skips the WRITE.
                    [env:UNDEFINED_VAR] WRITE signed-artifact.txt signed-content
                    ASSERT_ABSENT signed-artifact.txt
                "#},
            }]
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
            &[arg!(
                "substring",
                "string",
                "Expected substring in stdout",
                IoDirection::Read,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "verify stdout substring",
                fence_meta: None,
                code: indoc! {r#"
                    // Interpreter output and RUN child output both reach stdout.
                    ECHO build-complete
                    RUN echo artifact-built-ok
                    ASSERT_STDOUT build-complete
                    ASSERT_STDOUT artifact-built-ok
                "#},
            }]
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
            &[arg!(
                "path",
                "path",
                "File or directory to hash",
                IoDirection::Read,
                0,
                true,
                None
            )],
            &[],
            Some(Stream::Stdout),
            &[Example {
                name: "compute file hash",
                fence_meta: None,
                code: indoc! {r#"
                    WRITE payload.txt stable-content

                    // The digest is deterministic: sha256("stable-content").
                    HASH_SHA256 payload.txt
                    ASSERT_STDOUT 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c
                "#},
            }]
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
            &[arg!(
                "code",
                "int",
                "Exit status code",
                IoDirection::Write,
                0,
                true,
                None
            )],
            &[],
            None,
            &[Example {
                name: "exit with status code",
                fence_meta: Some("expect_error:\"EXIT requested with code 42\""),
                code: indoc! {r#"
                    // Teardown: background children are killed before the error.
                    WRITE teardown-order.txt background-children-killed-first
                    ASSERT_FILE teardown-order.txt background-children-killed-first

                    // Fails the script with "EXIT requested with code 42".
                    EXIT 42
                "#},
            }]
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
