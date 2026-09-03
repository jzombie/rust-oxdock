//! Single-site command registry for all OxDock commands.
//!
//! `declare_commands!` is the sole source of truth. It generates:
//! - StepKind enum — all command + structural AST variants
//! - `pub fn lower_command(name, raw_args)` — name-dispatched lowering
//! - `pub fn all_metadata()` — collects `CommandMeta` from all declarations
//!
//! To add a command: add one block inside `declare_commands!`.

use std::fmt;

use crate::ast::{Arg, Expr, IoBinding, IoStream, Step, WorkspaceTarget};
use crate::command::{ArgSpec, CommandMeta, Example, FlagSpec, FlagValueType, IoDirection, Stream};
use anyhow::{Result, anyhow, bail};
use indoc::indoc;

// ── Helpers ────────────────────────────────────────────────────────────────

fn join_args(args: Vec<Arg>, cmd_name: &str) -> Result<Arg> {
    if args.is_empty() {
        bail!("{cmd_name} requires at least one argument");
    }
    if args.len() == 1 {
        return Ok(args.into_iter().next().unwrap());
    }
    Ok(Arg::String(
        args.iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        false,
    ))
}

fn quote_arg(s: &str) -> String {
    let is_safe = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '/' || c == '.')
        && crate::Command::parse(s).is_none();
    if is_safe && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn quote_msg(s: &str) -> String {
    let safe = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && crate::Command::parse(s).is_none();
    if safe && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn quote_run(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c == ';' || c == '\n') || s.contains("//") {
        return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    }
    s.split(' ')
        .map(|w| {
            if w.starts_with(|c: char| c.is_ascii_digit())
                || w.starts_with(['/', '.', '-', ':', '='])
            {
                format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fmt_io(b: &IoBinding) -> String {
    let s = match b.stream {
        IoStream::Stdin => "stdin",
        IoStream::Stdout => "stdout",
        IoStream::Stderr => "stderr",
    };
    if let Some(p) = &b.pipe {
        format!("{}=pipe:{}", s, p)
    } else {
        s.to_string()
    }
}

// ── declare_commands! ──────────────────────────────────────────────────────

macro_rules! declare_commands {
    (
        structural [
            $( $sname:ident $( { $( $sfname:ident : $sftype:ty ),* $(,)? } )? ),* $(,)?
        ]

        $(
            $cmd_ident:ident => [
                name: $name:expr,
                variant: $vname:ident $( { $( $vfname:ident : $vftype:ty ),* $(,)? } )? $( ( $( $ttuple:ty ),* $(,)? ) )?,
                syntax: $syntax:expr,
                summary: $summary:expr,
                description: $desc:expr,
                args: $args:expr,
                flags: $flags:expr,
                default_output: $out:expr,
                examples: $examples:expr,
                lower: $lower:expr,
            ]
        ),* $(,)?
    ) => {
        #[derive(Debug, Clone, Eq, PartialEq)]
        pub enum StepKind {
            $( $vname $( { $( $vfname : $vftype ),* } )? $( ( $( $ttuple ),* ) )?, )*
            $( $sname $( { $( $sfname : $sftype ),* } )?, )*
        }

        pub fn lower_command(name: &str, raw_args: Vec<Arg>) -> Result<StepKind> {
            match name {
                $(
                    s if s == $name => {
                        let meta = CommandMeta {
                            name: $name, syntax: $syntax, summary: $summary,
                            description: $desc, args: $args, flags: $flags,
                            default_output: $out, examples: $examples,
                        };
                        let (flags, positional) = crate::strip_flags(raw_args, &meta)?;
                        let lower_fn: fn(Vec<(String, Arg)>, Vec<Arg>) -> Result<StepKind> = $lower;
                        lower_fn(flags, positional)
                    }
                )*
                _ => bail!("unknown command: {name}"),
            }
        }

        pub fn all_metadata() -> Vec<CommandMeta> {
            vec![
                $( CommandMeta {
                    name: $name, syntax: $syntax, summary: $summary,
                    description: $desc, args: $args, flags: $flags,
                    default_output: $out, examples: $examples,
                }, )*
            ]
        }
    };
}

declare_commands! {
    structural [
        WithIo { bindings: Vec<IoBinding>, cmd: Box<StepKind> },
        WithIoBlock { bindings: Vec<IoBinding> },
        For { key_var: Option<String>, var: String, in_expr: Expr, body: Vec<Step> },
        If { cond: Box<Expr>, then_body: Vec<Step>, else_ifs: Vec<(Box<Expr>, Vec<Step>)>, else_body: Option<Vec<Step>> },
        Assign { var: String, expr: Expr },
    ]

    Workdir => [
        name: "WORKDIR",
        variant: Workdir(Arg),
        syntax: "WORKDIR <path>",
        summary: "Change the working directory.",
        description: "Sets the current working directory.",
        args: &[ ArgSpec { name: "path", arg_type: "string", description: "Directory to change to", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "change working directory", fence_meta: None, code: indoc! {r#"
            WORKDIR project/src
            WRITE generated.txt generated-under-workdir
            ASSERT_FILE generated.txt generated-under-workdir
        "#} } ],
        lower: |_flags, args| {
            let path = args.into_iter().next().ok_or_else(|| anyhow!("WORKDIR requires a path"))?;
            Ok(StepKind::Workdir(path))
        },
    ],

    Workspace => [
        name: "WORKSPACE",
        variant: Workspace(WorkspaceTarget),
        syntax: "WORKSPACE SNAPSHOT|LOCAL",
        summary: "Switch workspace roots.",
        description: "SNAPSHOT or LOCAL root.",
        args: &[ ArgSpec { name: "target", arg_type: "SNAPSHOT|LOCAL", description: "Target root", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "switch roots", fence_meta: None, code: indoc! {r#"WORKSPACE LOCAL"#} } ],
        lower: |_flags, args| {
            let target = args.into_iter().next().ok_or_else(|| anyhow!("WORKSPACE requires a target"))?;
            match target.as_str() {
                "SNAPSHOT" | "snapshot" => Ok(StepKind::Workspace(WorkspaceTarget::Snapshot)),
                "LOCAL" | "local" => Ok(StepKind::Workspace(WorkspaceTarget::Local)),
                other => bail!("unknown workspace target: {other}"),
            }
        },
    ],

    Env => [
        name: "ENV",
        variant: Env { key: String, value: Arg },
        syntax: "ENV KEY=value",
        summary: "Set an environment variable.",
        description: "Inserts or updates an env var.",
        args: &[ ArgSpec { name: "assignment", arg_type: "KEY=value", description: "KEY=value pair", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "set env", fence_meta: None, code: indoc! {r#"ENV APP_MODE=production"#} } ],
        lower: |_flags, args| {
            let arg = args.into_iter().next().ok_or_else(|| anyhow!("ENV requires KEY=value"))?;
            let (k, v) = arg.as_str().split_once('=').ok_or_else(|| anyhow!("ENV requires KEY=value format"))?;
            let val = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
            Ok(StepKind::Env { key: k.to_string(), value: Arg::String(val.to_string(), false) })
        },
    ],

    InheritEnv => [
        name: "INHERIT_ENV",
        variant: InheritEnv { keys: Vec<String> },
        syntax: "INHERIT_ENV <key>...",
        summary: "Inherit env vars from host.",
        description: "Imports host env vars.",
        args: &[],
        flags: &[],
        default_output: None,
        examples: &[],
        lower: |_flags, args| {
            let keys = args.into_iter().map(|a| a.as_str().to_string()).collect();
            Ok(StepKind::InheritEnv { keys })
        },
    ],

    Echo => [
        name: "ECHO",
        variant: Echo(Arg),
        syntax: "ECHO <message>",
        summary: "Print to stdout.",
        description: "Outputs message to stdout.",
        args: &[ ArgSpec { name: "message", arg_type: "string", description: "Text", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "echo", fence_meta: None, code: indoc! {r#"ECHO build-complete"#} } ],
        lower: |_flags, args| Ok(StepKind::Echo(join_args(args, "ECHO")?)),
    ],

    Run => [
        name: "RUN",
        variant: Run(Arg),
        syntax: "RUN <command...>",
        summary: "Execute shell command.",
        description: "Runs command in cwd.",
        args: &[ ArgSpec { name: "command", arg_type: "string...", description: "Command", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "run", fence_meta: None, code: indoc! {r#"RUN echo hello"#} } ],
        lower: |_flags, args| Ok(StepKind::Run(join_args(args, "RUN")?)),
    ],

    RunBg => [
        name: "RUN_BG",
        variant: RunBg(Arg),
        syntax: "RUN_BG <command...>",
        summary: "Run in background.",
        description: "Like RUN but background.",
        args: &[ ArgSpec { name: "command", arg_type: "string...", description: "Command", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "bg", fence_meta: None, code: indoc! {r#"RUN_BG sleep 1"#} } ],
        lower: |_flags, args| Ok(StepKind::RunBg(join_args(args, "RUN_BG")?)),
    ],

    Copy => [
        name: "COPY",
        variant: Copy { from_current_workspace: bool, from: Arg, to: Arg },
        syntax: "COPY [--from-current-workspace] <from> <to>",
        summary: "Copy file into workspace.",
        description: "Copies from host.",
        args: &[
            ArgSpec { name: "from", arg_type: "path", description: "Source", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "to", arg_type: "path", description: "Dest", io: IoDirection::Write, index: 1, required: true, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "from_current_workspace", long: "--from-current-workspace", value_type: FlagValueType::Flag, required: false, description: "From workspace root" } ],
        default_output: None,
        examples: &[ Example { name: "copy", fence_meta: None, code: indoc! {r#"COPY src.txt dst.txt"#} } ],
        lower: |flags, args| {
            let from_current_workspace = flags.iter().any(|(k, _)| k == "from_current_workspace");
            let mut it = args.into_iter();
            let from = it.next().ok_or_else(|| anyhow!("COPY requires a source"))?;
            let to = it.next().ok_or_else(|| anyhow!("COPY requires a destination"))?;
            Ok(StepKind::Copy { from_current_workspace, from, to })
        },
    ],

    CopyGit => [
        name: "COPY_GIT",
        variant: CopyGit { rev: Arg, from: Arg, to: Arg, include_dirty: bool },
        syntax: "COPY_GIT [--include-dirty] <rev> <src> <dst>",
        summary: "Copy from git revision.",
        description: "Checkout and copy.",
        args: &[
            ArgSpec { name: "rev", arg_type: "string", description: "Rev", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "src", arg_type: "path", description: "Src", io: IoDirection::Read, index: 1, required: true, fallback_stream: None },
            ArgSpec { name: "dst", arg_type: "path", description: "Dst", io: IoDirection::Write, index: 2, required: true, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "dirty", long: "--include-dirty", value_type: FlagValueType::Flag, required: false, description: "Include dirty" } ],
        default_output: None,
        examples: &[ Example { name: "git copy", fence_meta: None, code: indoc! {r#"COPY_GIT HEAD src.txt dst.txt"#} } ],
        lower: |flags, args| {
            let include_dirty = flags.iter().any(|(k, _)| k == "dirty");
            let mut it = args.into_iter();
            let rev = it.next().ok_or_else(|| anyhow!("COPY_GIT requires a revision"))?;
            let from = it.next().ok_or_else(|| anyhow!("COPY_GIT requires a source"))?;
            let to = it.next().ok_or_else(|| anyhow!("COPY_GIT requires a destination"))?;
            Ok(StepKind::CopyGit { rev, from, to, include_dirty })
        },
    ],

    Symlink => [
        name: "SYMLINK",
        variant: Symlink { from: Arg, to: Arg },
        syntax: "SYMLINK <from> <to>",
        summary: "Create symlink.",
        description: "Creates symlink.",
        args: &[
            ArgSpec { name: "from", arg_type: "path", description: "Target", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "to", arg_type: "path", description: "Link", io: IoDirection::Write, index: 1, required: true, fallback_stream: None },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "symlink", fence_meta: None, code: indoc! {r#"SYMLINK original.txt link.txt"#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let from = it.next().ok_or_else(|| anyhow!("SYMLINK requires a source"))?;
            let to = it.next().ok_or_else(|| anyhow!("SYMLINK requires a target"))?;
            Ok(StepKind::Symlink { from, to })
        },
    ],

    Mkdir => [
        name: "MKDIR",
        variant: Mkdir(Arg),
        syntax: "MKDIR <path>",
        summary: "Create directory.",
        description: "Creates dir with parents.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "Dir path", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "mkdir", fence_meta: None, code: indoc! {r#"MKDIR deeply/nested/tree"#} } ],
        lower: |_flags, args| Ok(StepKind::Mkdir(args.into_iter().next().ok_or_else(|| anyhow!("MKDIR requires a path"))?)),
    ],

    Ls => [
        name: "LS",
        variant: Ls(Option<Arg>),
        syntax: "LS [<path>]",
        summary: "List directory.",
        description: "Lists entries.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "Dir", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "ls", fence_meta: None, code: indoc! {r#"LS inventory"#} } ],
        lower: |_flags, args| Ok(StepKind::Ls(args.into_iter().next())),
    ],

    Cwd => [
        name: "CWD",
        variant: Cwd,
        syntax: "CWD",
        summary: "Print working directory.",
        description: "Outputs cwd.",
        args: &[],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "cwd", fence_meta: None, code: indoc! {r#"CWD"#} } ],
        lower: |_flags, _args| Ok(StepKind::Cwd),
    ],

    Read => [
        name: "READ",
        variant: Read(Option<Arg>),
        syntax: "READ [<path>]",
        summary: "Read file to stdout.",
        description: "Outputs file contents.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "File", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "read", fence_meta: None, code: indoc! {r#"READ note.txt"#} } ],
        lower: |_flags, args| Ok(StepKind::Read(args.into_iter().next())),
    ],

    Write => [
        name: "WRITE",
        variant: Write { path: Arg, contents: Option<Arg> },
        syntax: "WRITE <path> [<contents>]",
        summary: "Write to file.",
        description: "Writes contents.",
        args: &[
            ArgSpec { name: "path", arg_type: "path", description: "File", io: IoDirection::Write, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "contents", arg_type: "string", description: "Content", io: IoDirection::Write, index: 1, required: false, fallback_stream: Some(Stream::Stdin) },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "write", fence_meta: None, code: indoc! {r#"WRITE output.txt hello-world"#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("WRITE requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_args(remaining, "WRITE")?) };
            Ok(StepKind::Write { path, contents })
        },
    ],

    Append => [
        name: "APPEND",
        variant: Append { path: Arg, contents: Option<Arg> },
        syntax: "APPEND <path> [<contents>]",
        summary: "Append to file.",
        description: "Appends contents.",
        args: &[
            ArgSpec { name: "path", arg_type: "path", description: "File", io: IoDirection::Write, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "contents", arg_type: "string", description: "Content", io: IoDirection::Write, index: 1, required: false, fallback_stream: Some(Stream::Stdin) },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "append", fence_meta: None, code: indoc! {r#"APPEND log.txt line2"#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("APPEND requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_args(remaining, "APPEND")?) };
            Ok(StepKind::Append { path, contents })
        },
    ],

    Expand => [
        name: "EXPAND",
        variant: Expand { path: Option<Arg>, overrides: Vec<(String, Arg)> },
        syntax: "EXPAND [<path>] [<KEY=val> ...]",
        summary: "Expand templates.",
        description: "Expands placeholders.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "Template", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "expand", fence_meta: None, code: indoc! {r#"EXPAND template.md NAME="Alice""#} } ],
        lower: |_flags, args| {
            let mut path = None;
            let mut overrides = Vec::new();
            for arg in args {
                let s = arg.as_str();
                if let Some((k, v)) = s.split_once('=') {
                    let val = v.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                        .unwrap_or(v);
                    overrides.push((k.to_string(), Arg::String(val.to_string(), false)));
                } else if path.is_none() { path = Some(arg); }
                else { bail!("EXPAND accepts at most one path"); }
            }
            Ok(StepKind::Expand { path, overrides })
        },
    ],

    AssertFile => [
        name: "ASSERT_FILE",
        variant: AssertFile { hash: Option<String>, path: Arg, contents: Option<Arg> },
        syntax: "ASSERT_FILE [--hash <sha256>] <path> [<expected>]",
        summary: "Assert file exists.",
        description: "Verifies file.",
        args: &[
            ArgSpec { name: "path", arg_type: "path", description: "File", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "expected", arg_type: "string", description: "Expected", io: IoDirection::Read, index: 1, required: false, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "hash", long: "--hash", value_type: FlagValueType::String, required: false, description: "SHA-256" } ],
        default_output: None,
        examples: &[ Example { name: "assert file", fence_meta: None, code: indoc! {r#"ASSERT_FILE payload.bin stable-content"#} } ],
        lower: |flags, args| {
            let hash = flags.iter().find(|(k, _)| k == "hash").map(|(_, v)| v.as_str().to_string());
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("ASSERT_FILE requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_args(remaining, "ASSERT_FILE")?) };
            Ok(StepKind::AssertFile { hash, path, contents })
        },
    ],

    AssertDir => [
        name: "ASSERT_DIR",
        variant: AssertDir(Arg),
        syntax: "ASSERT_DIR <path>",
        summary: "Assert dir exists.",
        description: "Verifies dir.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "Dir", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert dir", fence_meta: None, code: indoc! {r#"ASSERT_DIR dist/assets"#} } ],
        lower: |_flags, args| Ok(StepKind::AssertDir(args.into_iter().next().ok_or_else(|| anyhow!("ASSERT_DIR requires a path"))?)),
    ],

    AssertAbsent => [
        name: "ASSERT_ABSENT",
        variant: AssertAbsent(Arg),
        syntax: "ASSERT_ABSENT <path>",
        summary: "Assert path absent.",
        description: "Verifies absence.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "Path", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert absent", fence_meta: None, code: indoc! {r#"ASSERT_ABSENT missing.txt"#} } ],
        lower: |_flags, args| Ok(StepKind::AssertAbsent(args.into_iter().next().ok_or_else(|| anyhow!("ASSERT_ABSENT requires a path"))?)),
    ],

    AssertStdout => [
        name: "ASSERT_STDOUT",
        variant: AssertStdout(Arg),
        syntax: "ASSERT_STDOUT <substring>",
        summary: "Assert stdout contains.",
        description: "Verifies stdout.",
        args: &[ ArgSpec { name: "substring", arg_type: "string", description: "Substring", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert stdout", fence_meta: None, code: indoc! {r#"ASSERT_STDOUT build-complete"#} } ],
        lower: |_flags, args| Ok(StepKind::AssertStdout(join_args(args, "ASSERT_STDOUT")?)),
    ],

    HashSha256 => [
        name: "HASH_SHA256",
        variant: HashSha256 { path: Arg },
        syntax: "HASH_SHA256 <path>",
        summary: "Print SHA-256.",
        description: "Computes digest.",
        args: &[ ArgSpec { name: "path", arg_type: "path", description: "File", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "hash", fence_meta: None, code: indoc! {r#"HASH_SHA256 payload.txt"#} } ],
        lower: |_flags, args| Ok(StepKind::HashSha256 { path: args.into_iter().next().ok_or_else(|| anyhow!("HASH_SHA256 requires a path"))? }),
    ],

    Exit => [
        name: "EXIT",
        variant: Exit(i32),
        syntax: "EXIT <code>",
        summary: "Exit pipeline.",
        description: "Terminates.",
        args: &[ ArgSpec { name: "code", arg_type: "int", description: "Code", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "exit", fence_meta: None, code: indoc! {r#"EXIT 42"#} } ],
        lower: |_flags, args| {
            let code = args.into_iter().next().and_then(|a| a.as_str().parse::<i32>().ok()).unwrap_or(0);
            Ok(StepKind::Exit(code))
        },
    ],
}

// ── Display ────────────────────────────────────────────────────────────────

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepKind::InheritEnv { keys } => write!(f, "INHERIT_ENV [{}]", keys.join(", ")),
            StepKind::Workdir(a) => write!(f, "WORKDIR {}", quote_arg(a.as_str())),
            StepKind::Workspace(t) => write!(f, "WORKSPACE {}", t),
            StepKind::Env { key, value } => write!(f, "ENV {}={}", key, quote_arg(value.as_str())),
            StepKind::Run(c) => write!(f, "RUN {}", quote_run(c.as_str())),
            StepKind::Echo(m) => write!(f, "ECHO {}", quote_msg(m.as_str())),
            StepKind::RunBg(c) => write!(f, "RUN_BG {}", quote_run(c.as_str())),
            StepKind::Copy {
                from_current_workspace,
                from,
                to,
            } => {
                if *from_current_workspace {
                    write!(
                        f,
                        "COPY --from-current-workspace {} {}",
                        quote_arg(from.as_str()),
                        quote_arg(to.as_str())
                    )
                } else {
                    write!(
                        f,
                        "COPY {} {}",
                        quote_arg(from.as_str()),
                        quote_arg(to.as_str())
                    )
                }
            }
            StepKind::Symlink { from, to } => write!(
                f,
                "SYMLINK {} {}",
                quote_arg(from.as_str()),
                quote_arg(to.as_str())
            ),
            StepKind::Mkdir(a) => write!(f, "MKDIR {}", quote_arg(a.as_str())),
            StepKind::Ls(a) => {
                write!(f, "LS")?;
                if let Some(x) = a {
                    write!(f, " {}", quote_arg(x.as_str()))?;
                }
                Ok(())
            }
            StepKind::Cwd => write!(f, "CWD"),
            StepKind::Read(a) => {
                write!(f, "READ")?;
                if let Some(x) = a {
                    write!(f, " {}", quote_arg(x.as_str()))?;
                }
                Ok(())
            }
            StepKind::Write { path, contents } => {
                write!(f, "WRITE {}", quote_arg(path.as_str()))?;
                if let Some(b) = contents {
                    write!(f, " {}", quote_msg(b.as_str()))?;
                }
                Ok(())
            }
            StepKind::Append { path, contents } => {
                write!(f, "APPEND {}", quote_arg(path.as_str()))?;
                if let Some(b) = contents {
                    write!(f, " {}", quote_msg(b.as_str()))?;
                }
                Ok(())
            }
            StepKind::Expand { path, overrides } => {
                write!(f, "EXPAND")?;
                if let Some(p) = path {
                    write!(f, " {}", quote_arg(p.as_str()))?;
                }
                for (k, v) in overrides {
                    write!(f, " {}={}", k, quote_arg(v.as_str()))?;
                }
                Ok(())
            }
            StepKind::AssertFile {
                hash,
                path,
                contents,
            } => {
                if let Some(d) = hash {
                    write!(f, "ASSERT_FILE --hash {} {}", d, quote_arg(path.as_str()))
                } else {
                    write!(f, "ASSERT_FILE {}", quote_arg(path.as_str()))?;
                    if let Some(b) = contents {
                        write!(f, " {}", quote_msg(b.as_str()))?;
                    }
                    Ok(())
                }
            }
            StepKind::AssertDir(a) => write!(f, "ASSERT_DIR {}", quote_arg(a.as_str())),
            StepKind::AssertAbsent(a) => write!(f, "ASSERT_ABSENT {}", quote_arg(a.as_str())),
            StepKind::AssertStdout(m) => write!(f, "ASSERT_STDOUT {}", quote_msg(m.as_str())),
            StepKind::WithIo { bindings, cmd } => {
                let p: Vec<String> = bindings.iter().map(fmt_io).collect();
                write!(f, "WITH_IO [{}] {}", p.join(", "), cmd)
            }
            StepKind::WithIoBlock { bindings } => {
                let p: Vec<String> = bindings.iter().map(fmt_io).collect();
                write!(f, "WITH_IO [{}] {{...}}", p.join(", "))
            }
            StepKind::CopyGit {
                rev,
                from,
                to,
                include_dirty,
            } => {
                if *include_dirty {
                    write!(
                        f,
                        "COPY_GIT --include-dirty {} {} {}",
                        quote_arg(rev.as_str()),
                        quote_arg(from.as_str()),
                        quote_arg(to.as_str())
                    )
                } else {
                    write!(
                        f,
                        "COPY_GIT {} {} {}",
                        quote_arg(rev.as_str()),
                        quote_arg(from.as_str()),
                        quote_arg(to.as_str())
                    )
                }
            }
            StepKind::HashSha256 { path } => write!(f, "HASH_SHA256 {}", quote_arg(path.as_str())),
            StepKind::Exit(c) => write!(f, "EXIT {}", c),
            StepKind::For {
                key_var,
                var,
                in_expr,
                body,
            } => {
                match key_var {
                    Some(k) => write!(f, "FOR ${}, ${} IN {} {{", k, var, in_expr)?,
                    None => write!(f, "FOR ${} IN {} {{", var, in_expr)?,
                }
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::If {
                cond,
                then_body,
                else_ifs,
                else_body,
            } => {
                write!(f, "IF {} {{", cond)?;
                for s in then_body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, " }}")?;
                for (c, b) in else_ifs {
                    write!(f, " ELSE IF {} {{", c)?;
                    for s in b {
                        write!(f, "\n    {}", s)?;
                    }
                    write!(f, " }}")?;
                }
                if let Some(b) = else_body {
                    write!(f, " ELSE {{")?;
                    for s in b {
                        write!(f, "\n    {}", s)?;
                    }
                    write!(f, " }}")?;
                }
                Ok(())
            }
            StepKind::Assign { var, expr } => write!(f, "LET ${} = {}", var, expr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_script;

    #[test]
    fn verify_display_sync_with_metadata() {
        let registry = all_metadata();
        for meta in registry {
            if meta.examples.is_empty() {
                continue;
            }

            let code = meta.examples[0].code;
            let ast = parse_script(code, lower_command)
                .unwrap_or_else(|e| panic!("Failed to parse example for {}: {}", meta.name, e));

            if let Some(first) = ast.first() {
                let target_kind = match &first.kind {
                    StepKind::WithIo { cmd, .. } => &**cmd,
                    other => other,
                };

                let serialized = target_kind.to_string();

                assert!(
                    serialized.starts_with(meta.name),
                    "Display implementation mismatch!\nExpected prefix: {}\nActual serialization: {}",
                    meta.name,
                    serialized
                );
            }
        }
    }
}
