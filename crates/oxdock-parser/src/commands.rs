//! Single-site command registry for all OxDock commands.
//!
//! `declare_commands!` is the sole source of truth. It generates:
//! - StepKind enum — all command + structural AST variants
//! - `pub fn lower_command(name, raw_args)` — name-dispatched lowering
//! - `pub fn all_metadata()` — collects `CommandMeta` from all declarations
//!   plus `all_structural_metadata()` (structural statements are documented
//!   through the same pipeline so reference docs cannot drift).
//!
//! To add a command: add one block inside `declare_commands!`.
//! To add a structural statement: extend the `structural [...]` list,
//! `all_structural_metadata()`, and the `structural_metadata_covers_all_structural_kinds`
//! tripwire below.

use std::fmt;

use crate::ast::{Arg, ArgPart, Expr, IoBinding, IoStream, Step, WorkspaceTarget};
use crate::command::{
    ArgSpec, ArgType, CommandMeta, Example, FlagSpec, FlagValueType, IoDirection, Stream,
    split_assignment,
};
use anyhow::{Result, anyhow, bail};
use indoc::indoc;

// ── Helpers ────────────────────────────────────────────────────────────────

// Value-parsing helpers (`strip_surrounding_quotes`,
// `split_assignment`, `parse_duration`, `format_duration`) live in
// `crate::command` beside the `ArgType` validators that call them.

/// Join free-text tail arguments into one value. Single args pass through
/// untouched (preserving `Arg::Expr`); all-`String` tails join exactly like the
/// historical `join_args`; tails containing expressions become `Arg::Parts`
/// with single-space separators so `$x` is never silently dropped.
fn join_value(args: Vec<Arg>, cmd_name: &str) -> Result<Arg> {
    if args.is_empty() {
        bail!("{cmd_name} requires at least one argument");
    }
    if args.len() == 1 {
        return Ok(args.into_iter().next().unwrap());
    }
    if args.iter().all(|a| matches!(a, Arg::String(..))) {
        return Ok(Arg::String(
            args.iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            false,
        ));
    }
    let mut parts = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        if index > 0 {
            parts.push(ArgPart::Text(" ".to_string(), false));
        }
        match arg {
            Arg::String(text, quoted) => parts.push(ArgPart::Text(text, quoted)),
            Arg::Expr(expr) => parts.push(ArgPart::Expr(expr)),
            Arg::Parts(inner) => parts.extend(inner),
        }
    }
    Ok(Arg::Parts(parts))
}

/// Canonical `lower_command` entry for direct callers holding one pre-joined
/// `KEY=value` token. Script parsing never reaches this — the grammar splits
/// assignments on raw spans first (see `lower_env_command` in parser.rs).
pub fn lower_env_assignment(args: Vec<Arg>) -> Result<StepKind> {
    let arg = args
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("ENV requires KEY=value"))?;
    let Some((key, value)) = split_assignment(arg.as_str())? else {
        bail!("ENV requires KEY=value format")
    };
    Ok(StepKind::Env { key, value })
}

/// Collapse a grammar-classified assignment for commands that take no
/// assignments (`RUN`, `COPY`, ...): canonical `key=<rendered value>` text.
/// Runtime semantics survive intact — `{{ }}` templates stay textual for
/// `expand_string`, and `RUN`'s own post-pass expands bare `$var`.
pub(crate) fn canonical_assignment_arg(key: &str, value: &Arg) -> Arg {
    Arg::String(format!("{key}={}", value.render()), false)
}

/// Render one `Arg` for `Display`: expressions print raw (`$x` must never be
/// quoted or reparsing would literalize them); mixed values print raw unless
/// they hold instruction-boundary characters (`;`, `}`, linebreaks), which
/// force quoting for reparseability.
fn fmt_value(arg: &Arg, quote: fn(&str) -> String) -> String {
    match arg {
        Arg::Expr(_) => arg.render(),
        Arg::String(text, _) => quote(text),
        Arg::Parts(_) => {
            let rendered = arg.render();
            if rendered.contains(';')
                || rendered.contains('}')
                || rendered.contains('\n')
                || rendered.contains('\r')
            {
                quote(&rendered)
            } else {
                rendered
            }
        }
    }
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

/// Render one exec-form (`RUN [...]`) argv element for `Display`:
/// string literals print JSON-quoted; typed expressions (`$var`,
/// `CALL()`, ints, bools, nested lists) print raw via `render` so
/// reparsing yields the same typed element; mixed values print raw
/// unless they hold instruction-boundary characters.
fn fmt_exec_arg(arg: &Arg) -> String {
    match arg {
        Arg::String(text, _) => {
            format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
        }
        Arg::Expr(_) => arg.render(),
        Arg::Parts(_) => {
            let rendered = arg.render();
            if rendered.contains(';')
                || rendered.contains('}')
                || rendered.contains('\n')
                || rendered.contains('\r')
            {
                format!(
                    "\"{}\"",
                    rendered.replace('\\', "\\\\").replace('"', "\\\"")
                )
            } else {
                rendered
            }
        }
    }
}

/// Render an [`Arg`] for `Display`: the quoted flag drives quoting (not
/// content sniffing — digit-leading values like `10s` or `0` must stay
/// bare to reparse with the same flag).
fn fmt_raw_arg(arg: &Arg) -> String {
    match arg {
        Arg::String(s, true) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        _ => arg.render(),
    }
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

// Keywords parsed by PEG rules rather than plain-command lowering (`WITH_IO`,
// `AWAIT`, ...). When a line starts with one of these but fails to parse as
// such, lowering falls through here — report a committed syntax error instead
// of an unknown command.
fn is_known_command(name: &str) -> bool {
    if name == "ELSE" {
        return true;
    }
    all_metadata().iter().any(|meta| meta.name == name)
}

pub(crate) fn invalid_syntax_error(name: &str, raw_args: &[Arg]) -> anyhow::Error {
    let received = raw_args
        .iter()
        .map(Arg::render)
        .collect::<Vec<_>>()
        .join(" ");
    let got = if received.is_empty() {
        "nothing".to_string()
    } else {
        format!("`{received}`")
    };
    match structural_hint(name, &received) {
        Some(hint) => anyhow!("invalid syntax for command {name}: {hint}"),
        None => anyhow!("invalid syntax for command {name}: got {got}."),
    }
}

fn unknown_command_error(name: &str, raw_args: &[Arg]) -> anyhow::Error {
    let received = raw_args
        .iter()
        .map(Arg::render)
        .collect::<Vec<_>>()
        .join(" ");
    let hint = structural_hint(name, &received).or_else(|| case_hint(name));
    match hint {
        Some(hint) => anyhow!("unknown command: {name}\n{hint}"),
        None => anyhow!("unknown command: {name}"),
    }
}

fn structural_hint(name: &str, received: &str) -> Option<String> {
    let got = if received.is_empty() {
        "nothing".to_string()
    } else {
        format!("`{received}`")
    };
    match name {
        "WITH_IO" => Some(with_io_hint(&got, received)),
        "AWAIT" => Some(format!(
            "AWAIT waits for a background task variable, e.g. `LET $t = ASYNC ECHO hi` then `AWAIT $t`; got {got}."
        )),
        "CANCEL" => Some(format!(
            "CANCEL stops a background task variable, e.g. `CANCEL $t` (from `LET $t = ASYNC ...`); got {got}."
        )),
        "ASYNC" => Some(format!(
            "ASYNC runs a command in the background, e.g. `ASYNC RUN ...`, `ASYNC {{ ... }}`, or `LET $t = ASYNC ...`; got {got}."
        )),
        "FOR" => Some(format!(
            "FOR loops need `FOR $item IN <expr> {{ ... }}` (or `FOR $key, $value IN <expr> {{ ... }}`); got {got}."
        )),
        "IF" => Some(format!(
            "IF needs a condition and a block, e.g. `IF true {{ ECHO yes }}`; got {got}."
        )),
        "ELSE" => Some(format!(
            "ELSE must directly follow an `IF ... {{ ... }}` block, e.g. `IF true {{ ECHO yes }} ELSE {{ ECHO no }}`; got {got}."
        )),
        "LET" => Some(format!(
            "LET assigns a variable, e.g. `LET $name = <expr>` or `LET $t = ASYNC ...`; got {got}."
        )),
        "TIMEOUT" => Some(format!(
            "TIMEOUT needs a duration and a command or block, e.g. `TIMEOUT 30s RUN ...`; got {got}."
        )),
        "INHERIT_ENV" => Some(format!(
            "INHERIT_ENV takes a key list, e.g. `INHERIT_ENV [HOME PATH]`; got {got}."
        )),
        _ => None,
    }
}

/// Diagnose a `WITH_IO` line that failed to parse: most often a malformed
/// binding list (bindings are bare streams or `<stream>=pipe:<name>`).
fn with_io_hint(got: &str, received: &str) -> String {
    const SYNTAX: &str =
        "WITH_IO needs `WITH_IO [bindings] <command>` or `WITH_IO [bindings] { <commands> }`";
    const BINDINGS: &str = "bindings are `stdin`, `stdout`, `stderr`, or `<stream>=pipe:<name>` (e.g. `[stdout=pipe:log]`)";
    if let Some(after_open) = received.strip_prefix('[') {
        match after_open.split_once(']') {
            None => {
                return format!("{SYNTAX}: missing closing `]` in the binding list; got {got}.");
            }
            Some((bindings, _)) => {
                for part in bindings.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let (stream, binding) = match part.split_once('=') {
                        Some((stream, binding)) => (stream.trim(), Some(binding.trim())),
                        None => (part, None),
                    };
                    if !matches!(stream, "stdin" | "stdout" | "stderr") {
                        return format!(
                            "{SYNTAX}: invalid stream `{stream}`; expected `stdin`, `stdout`, or `stderr`; got {got}."
                        );
                    }
                    let valid = match binding {
                        None => true,
                        Some(value) => value
                            .strip_prefix("pipe:")
                            .map(|pipe| !pipe.trim().is_empty())
                            .unwrap_or(false),
                    };
                    if !valid {
                        return format!(
                            "{SYNTAX}: invalid binding `{part}`; {BINDINGS}; got {got}."
                        );
                    }
                }
            }
        }
    }
    format!("{SYNTAX}; got {got}. {BINDINGS}.")
}

/// `echo hi` is almost certainly `ECHO hi`: commands are uppercase.
fn case_hint(name: &str) -> Option<String> {
    let upper = name.to_ascii_uppercase();
    if upper != name
        && all_metadata()
            .iter()
            .any(|meta| meta.name == upper.as_str())
    {
        return Some(format!("did you mean `{upper}`? commands are uppercase."));
    }
    None
}

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
                        crate::command::validate_positionals_against_meta(
                            s,
                            &meta.args,
                            &positional,
                        )?;
                        let lower_fn: fn(Vec<(String, Arg)>, Vec<Arg>) -> Result<StepKind> = $lower;
                        lower_fn(flags, positional)
                    }
                )*
                _ => {
                    if is_known_command(name) {
                        Err(invalid_syntax_error(name, &raw_args))
                    } else {
                        Err(unknown_command_error(name, &raw_args))
                    }
                }
            }
        }

        pub fn all_metadata() -> Vec<CommandMeta> {
            let mut out = vec![
                $( CommandMeta {
                    name: $name, syntax: $syntax, summary: $summary,
                    description: $desc, args: $args, flags: $flags,
                    default_output: $out, examples: $examples,
                }, )*
            ];
            // Structural statements are registered separately (see
            // all_structural_metadata) but documented through the same
            // pipeline so docs-gen never drifts from the parser.
            out.extend(all_structural_metadata());
            out
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
        AsyncBlock { body: Vec<Step> },
        AssignAsync { var: String, body: Vec<Step> },
        Await { var: String },
        Cancel { var: String },
        Timeout { duration: Arg, body: Vec<Step> },
        RunExec { argv: Vec<Arg> },
    ]

    Workdir => [
        name: "WORKDIR",
        variant: Workdir(Arg),
        syntax: "WORKDIR <path>",
        summary: "Change the working directory.",
        description: "Sets the current working directory. Relative paths resolve against the current directory; `/` resets to the workspace root. Paths cannot escape the workspace.",
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Directory to change to", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
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
        args: &[ ArgSpec { name: "target", arg_type: ArgType::OneOf(&["SNAPSHOT", "LOCAL"]), description: "Target root", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
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
        description: "Inserts or updates an env var. The value uses the unified string-value rules shared by every command: `\"...\"` or `'...'` quotes keep exact bytes (spaces, tabs), a lone `$var` evaluates that variable, `{{ ... }}` placeholders interpolate, unquoted words join with single spaces, and the first `=` splits key from value (`KEY=a=b` stores `a=b`). A `$var` inside larger text stays literal — write `{{ $var }}` to interpolate there.",
        args: &[ ArgSpec { name: "assignment", arg_type: ArgType::KeyValue, description: "KEY=value pair", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[
            Example { name: "set env", fence_meta: None, code: indoc! {r#"ENV APP_MODE=production"#} },
            Example { name: "quoted value with spaces", fence_meta: None, code: indoc! {r#"
                # quotes keep the space: SET_FORTH stores `outer scope`
                ENV SET_FORTH="outer scope"
                WRITE out.txt "{{ env:SET_FORTH }}"
                ASSERT_FILE out.txt "outer scope"
            "#} },
            Example { name: "variable value", fence_meta: None, code: indoc! {r#"
                # a lone $var evaluates, like ECHO $var
                LET $who = "Alice"
                ENV GREETING=$who
                WRITE out.txt "{{ env:GREETING }}"
                ASSERT_FILE out.txt "Alice"
            "#} },
            Example { name: "all value forms agree", fence_meta: None, code: indoc! {r#"
                # a bare variable, a quoted literal, and a template all
                # store plain strings through the same value rules
                LET $x = "Ada"
                ENV A=$x
                ENV B="hello world"
                ENV C="{{ $x }} concatenated"
                WRITE check.txt "{{ env:A }}|{{ env:B }}|{{ env:C }}"
                ASSERT_FILE check.txt "Ada|hello world|Ada concatenated"
            "#} },
            Example { name: "scoped env reverts", fence_meta: None, code: indoc! {r#"
                # ENV inside a braced block reverts when the block exits
                ENV MODE=production
                [bool:true] {
                    ENV MODE=staging
                    WRITE inner.txt "{{ env:MODE }}"
                }
                WRITE outer.txt "{{ env:MODE }}"
                ASSERT_FILE inner.txt "staging"
                ASSERT_FILE outer.txt "production"
            "#} },
        ],
        lower: |_flags, args| lower_env_assignment(args),
    ],

    InheritEnv => [
        name: "INHERIT_ENV",
        variant: InheritEnv { keys: Vec<String> },
        syntax: "INHERIT_ENV <key>...",
        summary: "Inherit env vars from host.",
        description: "Declares which host environment variables to inherit into the script. Must appear before any other commands and at most once. Without this directive, the script starts with an empty environment.",
        args: &[ ArgSpec { name: "keys", arg_type: ArgType::Rest(&ArgType::String), description: "Host variables to inherit", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "inherit env", fence_meta: None, code: indoc! {r#"INHERIT_ENV [PATH, HOME]"#} } ],
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
        args: &[ ArgSpec { name: "message", arg_type: ArgType::Rest(&ArgType::String), description: "Text", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[
            Example { name: "echo", fence_meta: None, code: indoc! {r#"ECHO build-complete"#} },
            Example { name: "variables", fence_meta: None, code: indoc! {r#"
                # a lone $x evaluates; {{ }} interpolates inside text
                LET $x = "World"
                ECHO {{ $x }}
                ECHO $x
                ASSERT_STDOUT "World"
            "#} },
        ],
        lower: |_flags, args| Ok(StepKind::Echo(join_value(args, "ECHO")?)),
    ],

    Run => [
        name: "RUN",
        variant: Run(Arg),
        syntax: "RUN <command...> | RUN [\"exe\", \"arg\", ...]",
        summary: "Execute shell command or direct executable.",
        description: "Shell form (`RUN <command...>`) runs the joined command string in the system shell (`$SHELL -c` / `COMSPEC /C`). Exec form (`RUN [\"exe\", \"arg\", ...]`) spawns the executable directly with no shell, so there is no shell expansion, globbing, redirection, or pipes; use it for portable commands. Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`, `LET`) apply to both forms.",
        args: &[ ArgSpec { name: "command", arg_type: ArgType::Rest(&ArgType::String), description: "Command", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "run", fence_meta: None, code: indoc! {r#"RUN echo hello"#} }, Example { name: "run exec form", fence_meta: None, code: indoc! {r#"RUN ["cargo", "--version"]"#} } ],
        lower: |_flags, args| match args.as_slice() {
            [Arg::Expr(Expr::List(elems))] if elems.is_empty() => {
                bail!("RUN requires at least one argument")
            }
            [Arg::Expr(Expr::List(elems))] => Ok(StepKind::RunExec {
                argv: elems.iter().cloned().map(Arg::Expr).collect(),
            }),
            _ => Ok(StepKind::Run(join_value(args, "RUN")?)),
        },
    ],

    Copy => [
        name: "COPY",
        variant: Copy { from_current_workspace: bool, from: Arg, to: Arg },
        syntax: "COPY [--from-current-workspace] <from> <to>",
        summary: "Copy file into workspace.",
        description: "Copies from host.",
        args: &[
            ArgSpec { name: "from", arg_type: ArgType::Path, description: "Source", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "to", arg_type: ArgType::Path, description: "Dest", io: IoDirection::Write, index: 1, required: true, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "from_current_workspace", long: "--from-current-workspace", value_type: FlagValueType::Flag, required: false, description: "Copy from workspace instead of build context" } ],
        default_output: None,
        examples: &[ Example { name: "copy", fence_meta: Some("roots:unified"), code: indoc! {r#"
            WRITE src.txt content
            COPY src.txt dst.txt
            ASSERT_FILE dst.txt content
        "#} }, Example { name: "copy from workspace", fence_meta: Some("roots:unified"), code: indoc! {r#"
            WRITE ws-src.txt ws-content
            COPY --from-current-workspace ws-src.txt ws-copy.txt
            ASSERT_FILE ws-copy.txt ws-content
        "#} } ],
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
            ArgSpec { name: "rev", arg_type: ArgType::String, description: "Rev", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "src", arg_type: ArgType::Path, description: "Src", io: IoDirection::Read, index: 1, required: true, fallback_stream: None },
            ArgSpec { name: "dst", arg_type: ArgType::Path, description: "Dst", io: IoDirection::Write, index: 2, required: true, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "dirty", long: "--include-dirty", value_type: FlagValueType::Flag, required: false, description: "Include dirty" } ],
        default_output: None,
        examples: &[ Example { name: "git copy", fence_meta: Some("expect_error:\"COPY source missing\""), code: indoc! {r#"COPY_GIT HEAD src.txt dst.txt"#} } ],
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
            ArgSpec { name: "from", arg_type: ArgType::Path, description: "Target", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "to", arg_type: ArgType::Path, description: "Link", io: IoDirection::Write, index: 1, required: true, fallback_stream: None },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "symlink", fence_meta: Some("roots:unified"), code: indoc! {r#"
            WRITE original.txt content
            SYMLINK original.txt link.txt
            ASSERT_FILE link.txt content
        "#} } ],
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
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Dir path", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
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
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Dir", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "ls", fence_meta: None, code: indoc! {r#"
            MKDIR inventory
            WRITE inventory/a.txt a
            LS inventory
        "#} } ],
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
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "File", io: IoDirection::Read, index: 0, required: false, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "read", fence_meta: None, code: indoc! {r#"
            WRITE note.txt "hello"
            READ note.txt
        "#} } ],
        lower: |_flags, args| Ok(StepKind::Read(args.into_iter().next())),
    ],

    ReadLine => [
        name: "READ_LINE",
        variant: ReadLine { var: String },
        syntax: "READ_LINE $var",
        summary: "Read one line from stdin into a variable.",
        description: "Reads bytes until newline without waiting for EOF, leaving the pipe open. Trailing newline is stripped (shell-read parity). On premature EOF assigns accumulated bytes and returns.",
        args: &[ ArgSpec { name: "var", arg_type: ArgType::Var, description: "Variable to store the line", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "read line", fence_meta: None, code: indoc! {r#"
            WITH_IO [stdout=pipe:lines] ECHO "first"
            WITH_IO [stdin=pipe:lines] READ_LINE $reply
        "#} } ],
        lower: |_flags, args| {
            let arg = args.into_iter().next().ok_or_else(|| anyhow!("READ_LINE requires a variable"))?;
            let var = match arg {
                Arg::Expr(Expr::Var(name)) => name,
                Arg::String(s, _) => s.trim_start_matches('$').to_string(),
                other => bail!("READ_LINE requires a $variable, found {:?}", other),
            };
            if var.is_empty() {
                bail!("READ_LINE requires a variable");
            }
            Ok(StepKind::ReadLine { var })
        },
    ],

    Write => [
        name: "WRITE",
        variant: Write { path: Arg, contents: Option<Arg> },
        syntax: "WRITE <path> [<contents>]",
        summary: "Write to file.",
        description: "Writes contents.",
        args: &[
            ArgSpec { name: "path", arg_type: ArgType::Path, description: "File", io: IoDirection::Write, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "contents", arg_type: ArgType::Rest(&ArgType::String), description: "Content", io: IoDirection::Write, index: 1, required: false, fallback_stream: Some(Stream::Stdin) },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "write", fence_meta: None, code: indoc! {r#"WRITE output.txt hello-world"#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("WRITE requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_value(remaining, "WRITE")?) };
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
            ArgSpec { name: "path", arg_type: ArgType::Path, description: "File", io: IoDirection::Write, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "contents", arg_type: ArgType::Rest(&ArgType::String), description: "Content", io: IoDirection::Write, index: 1, required: false, fallback_stream: Some(Stream::Stdin) },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "append", fence_meta: None, code: indoc! {r#"
            WRITE log.txt line1
            APPEND log.txt line2
            ASSERT_FILE log.txt line1line2
        "#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("APPEND requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_value(remaining, "APPEND")?) };
            Ok(StepKind::Append { path, contents })
        },
    ],

    Expand => [
        name: "EXPAND",
        variant: Expand { path: Option<Arg>, overrides: Vec<(String, Arg)> },
        syntax: "EXPAND [<path>] [<KEY=val> ...]",
        summary: "Expand a template file (or stdin) to stdout.",
        description: "A template is any text file — or piped stdin when no path is given — containing `{{ ... }}` placeholders. EXPAND replaces each placeholder and prints the result to stdout. Placeholders: `{{ NAME }}` reads a `KEY=val` override passed on this command; `{{ env:NAME }}` reads an override, falling back to the environment; `{{ $var }}` reads a script variable (dotted paths allowed). A missing key is an error, never a silent empty. Substitution runs in a single pass. EXPAND is not recursive and does not expand nested placeholders: a value that itself contains `{{ ... }}` is inserted verbatim and never expanded again. A bare `$var` argument is a template path; `KEY=val` arguments are overrides whose values follow the unified string-value rules (same as `ENV`: quotes keep exact bytes, a lone `$var` evaluates, `{{ ... }}` interpolates). NOTE: `WRITE` interpolates `{{ ... }}` while writing, so escape it (`\\{{ ... }}`) when writing a template file for a later `EXPAND`. With no path, the template arrives on stdin through a pipe. When piping from a shell, single-quote the template (`echo '{{ $x }}'`): double quotes let the shell swallow `$x`, so oxdock receives an empty `{{ }}` placeholder and errors.",
        args: &[
            ArgSpec { name: "path", arg_type: ArgType::Path, description: "Template file to expand; omit to expand stdin", io: IoDirection::Read, index: 0, required: false, fallback_stream: None },
            ArgSpec { name: "overrides", arg_type: ArgType::Rest(&ArgType::KeyValue), description: "Template overrides shadowing that key (unified string values)", io: IoDirection::Read, index: 1, required: false, fallback_stream: None },
        ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[
            Example { name: "expand", fence_meta: None, code: indoc! {r#"
                ENV NAME="Alice"
                WRITE template.md "Hello {{ env:NAME }}!"
                EXPAND template.md
                ASSERT_STDOUT "Hello Alice!"
            "#} },
            Example { name: "override with spaces", fence_meta: None, code: indoc! {r#"
                # WRITE would interpolate {{ }} right away, so escape it:
                # the file must literally contain {{ env:NAME }} for EXPAND
                WRITE template.md "Hello \{{ env:NAME }}!"
                EXPAND template.md NAME="Alice Smith"
                ASSERT_STDOUT "Hello Alice Smith!"
            "#} },
            Example { name: "variable override", fence_meta: None, code: indoc! {r#"
                # same escaping: keep the placeholder literal until EXPAND;
                # a lone $who evaluates, like ECHO $who
                LET $who = "Bob"
                WRITE template.md "Hi \{{ env:WHO }}!"
                EXPAND template.md WHO=$who
                ASSERT_STDOUT "Hi Bob!"
            "#} },
            Example { name: "override forms agree", fence_meta: None, code: indoc! {r#"
                # a bare variable and a template-with-tail expand identically
                LET $x = "Ada"
                WRITE template.md "Hi \{{ env:NAME }} and \{{ env:NAME2 }}!"
                EXPAND template.md NAME=$x NAME2="{{ $x }} concatenated"
                ASSERT_STDOUT "Hi Ada and Ada concatenated!"
            "#} },
            Example { name: "expand stdin", fence_meta: None, code: indoc! {r#"
                # no path: the template arrives on stdin through a pipe
                WITH_IO [stdout=pipe:tpl] ECHO "Hello \{{ env:NAME }}!"
                WITH_IO [stdin=pipe:tpl] EXPAND NAME=Alice
                ASSERT_STDOUT "Hello Alice!"
            "#} },
            Example { name: "override does not leak", fence_meta: None, code: indoc! {r#"
                # KEY=val overrides shadow env for that EXPAND only —
                # they never update the environment itself
                ENV NAME="Alice"
                WRITE template.md "Hi \{{ env:NAME }}!"
                EXPAND template.md NAME="Bob"
                ASSERT_STDOUT "Hi Bob!"
                EXPAND template.md
                ASSERT_STDOUT "Hi Alice!"
            "#} },
        ],
        lower: |_flags, args| {
            let mut path = None;
            let mut overrides = Vec::new();
            for arg in args {
                let text = arg.as_str();
                if let Some((key, value)) = split_assignment(text)? {
                    overrides.push((key, value));
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
        description: "Checks the path is a file, then optionally compares its bytes (or `--hash` SHA-256 digest) against the expectation. Any mismatch aborts the pipeline with a step-numbered error showing expected vs actual.",
        args: &[
            ArgSpec { name: "path", arg_type: ArgType::Path, description: "File", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "expected", arg_type: ArgType::Rest(&ArgType::String), description: "Expected", io: IoDirection::Read, index: 1, required: false, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "hash", long: "--hash", value_type: FlagValueType::String, required: false, description: "SHA-256" } ],
        default_output: None,
        examples: &[ Example { name: "assert file", fence_meta: None, code: indoc! {r#"
            WRITE payload.bin stable-content
            ASSERT_FILE payload.bin stable-content
        "#} },
        Example { name: "assert file hash", fence_meta: None, code: indoc! {r#"
            # --hash compares the SHA-256 digest instead of raw bytes
            WRITE payload.bin stable-content
            ASSERT_FILE --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c payload.bin
        "#} } ],
        lower: |flags, args| {
            let hash = flags.iter().find(|(k, _)| k == "hash").map(|(_, v)| v.as_str().to_string());
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| anyhow!("ASSERT_FILE requires a path"))?;
            let remaining: Vec<Arg> = it.collect();
            let contents = if remaining.is_empty() { None } else { Some(join_value(remaining, "ASSERT_FILE")?) };
            Ok(StepKind::AssertFile { hash, path, contents })
        },
    ],

    AssertDir => [
        name: "ASSERT_DIR",
        variant: AssertDir(Arg),
        syntax: "ASSERT_DIR <path>",
        summary: "Assert dir exists.",
        description: "Checks the path is a directory, aborting the pipeline with a step-numbered error otherwise.",
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Dir", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert dir", fence_meta: None, code: indoc! {r#"
            MKDIR dist/assets
            ASSERT_DIR dist/assets
        "#} } ],
        lower: |_flags, args| Ok(StepKind::AssertDir(args.into_iter().next().ok_or_else(|| anyhow!("ASSERT_DIR requires a path"))?)),
    ],

    AssertAbsent => [
        name: "ASSERT_ABSENT",
        variant: AssertAbsent(Arg),
        syntax: "ASSERT_ABSENT <path>",
        summary: "Assert path absent.",
        description: "Checks nothing exists at the path, aborting the pipeline with a step-numbered error if it does.",
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Path", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
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
        description: "Checks the preceding step's stdout contains the substring, aborting the pipeline with a step-numbered error otherwise.",
        args: &[ ArgSpec { name: "substring", arg_type: ArgType::Rest(&ArgType::String), description: "Substring", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert stdout", fence_meta: None, code: indoc! {r#"
            ECHO build-complete
            ASSERT_STDOUT build-complete
        "#} } ],
        lower: |_flags, args| Ok(StepKind::AssertStdout(join_value(args, "ASSERT_STDOUT")?)),
    ],

    HashSha256 => [
        name: "HASH_SHA256",
        variant: HashSha256 { path: Arg },
        syntax: "HASH_SHA256 <path>",
        summary: "Print SHA-256.",
        description: "Computes digest.",
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "File", io: IoDirection::Read, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: Some(Stream::Stdout),
        examples: &[ Example { name: "hash", fence_meta: None, code: indoc! {r#"
            WRITE payload.txt hello
            HASH_SHA256 payload.txt
        "#} } ],
        lower: |_flags, args| Ok(StepKind::HashSha256 { path: args.into_iter().next().ok_or_else(|| anyhow!("HASH_SHA256 requires a path"))? }),
    ],

    Exit => [
        name: "EXIT",
        variant: Exit(Arg),
        syntax: "EXIT <code>",
        summary: "Exit pipeline.",
        description: "Stops the pipeline immediately with an `EXIT requested with code <code>` error; steps after it never run, at any nesting depth. Enclosing blocks still unwind their LET/ENV/WORKDIR/WORKSPACE state, anonymous background tasks are killed synchronously, and files written before the EXIT persist.",
        args: &[ ArgSpec { name: "code", arg_type: ArgType::Int, description: "Code", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "exit", fence_meta: Some("expect_error:\"EXIT requested with code 0\""), code: indoc! {r#"EXIT 0"#} } ],
        lower: |_flags, args| {
            // Static literals were already Int-checked by the central
            // validator; dynamics resolve (and validate) at runtime.
            let code = args.into_iter().next().ok_or_else(|| anyhow!("EXIT requires a code"))?;
            Ok(StepKind::Exit(code))
        },
    ],

    Sleep => [
        name: "SLEEP",
        variant: Sleep { duration: Arg },
        syntax: "SLEEP <duration>",
        summary: "Pause execution for a duration.",
        description: "Parks the step for the duration (e.g. 500ms, 10s, 2m). Cooperative: checks for cancellation so an enclosing TIMEOUT or task teardown interrupts the sleep. Cross-platform alternative to shell sleep for testing time boundaries.",
        args: &[ ArgSpec { name: "duration", arg_type: ArgType::Duration, description: "How long to sleep", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[
            Example { name: "sleep", fence_meta: None, code: indoc! {r#"SLEEP 100ms"#} },
            Example {
                name: "sleep variable duration",
                fence_meta: None,
                code: indoc! {r#"
                # durations resolve at runtime, so variables work too —
                # quoted or bare, both bind the same string
                LET $pause = "100ms"
                SLEEP $pause
                LET $bare = 100ms
                SLEEP $bare
            "#},
            },
        ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let raw = it
                .next()
                .ok_or_else(|| anyhow!("SLEEP requires a duration (e.g. SLEEP 500ms)"))?;
            if it.next().is_some() {
                bail!("SLEEP takes exactly one duration argument");
            }
            // Static literals were Duration-checked by the central
            // validator; dynamics ($var, templates) resolve at runtime.
            Ok(StepKind::Sleep { duration: raw })
        },
    ],
}

// ── Structural metadata ──────────────────────────────────────────────────
// Single source of truth for structural-statement documentation (TIMEOUT,
// ASYNC, AWAIT, WITH_IO, IF, FOR, ...). These constructs are parsed by PEG
// rules rather than `declare_commands!`, so their reference docs live here
// instead of `crates/docs-gen/src/command_ref.rs` — adding a structural
// StepKind without registering it here fails `structural_metadata_covers_all_structural_kinds`
// below, and docs-gen renders these entries dynamically (no hardcoded copy).
pub fn all_structural_metadata() -> Vec<CommandMeta> {
    vec![
        CommandMeta {
            name: "WITH_IO",
            syntax: "WITH_IO [bindings] <command> | WITH_IO [bindings] { <commands> }",
            summary: "Reroute standard streams.",
            description: "Reroutes the standard streams of the next command or, in block form, of every enclosed command. Bindings map streams (`stdin`, `stdout`, `stderr`) to named pipes (`stdout=pipe:name`). Pipe names registered by the host runtime tee structured output elsewhere; a name bound as output can later feed another command's `stdin`, connecting commands without touching the terminal. Nested blocks stack defaults; inline bindings override inherited ones for their command only; closing a block restores previous wiring.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "with_io block",
                fence_meta: None,
                code: indoc! {r#"
                WITH_IO [stdout=pipe:log] {
                  ECHO first
                  ECHO second
                }
                WITH_IO [stdin=pipe:log] WRITE captured.txt
            "#},
            }],
        },
        CommandMeta {
            name: "FOR",
            syntax: "FOR $item IN <expr> { <commands> } | FOR $key, $value IN <expr> { <commands> }",
            summary: "Iterate over a list or map.",
            description: "The loop variable receives each element (lists) or value (maps); with two variables, the first receives the key. Loop variables are scoped to the loop body and do not leak outward. The body may be a braced block or a single-line `{ ... }` command. `GLOB(\"...\")` patterns must be quoted (`*` is not a bare word, so `GLOB(*)` is a parse error); GLOB returns a root-relative sorted list, empty when nothing matches, and rejects `..` escapes.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "for loop",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $items = ["a", "b"]
                FOR $item IN $items {
                  ECHO $item
                }

                LET $map = {"x": 1}
                FOR $k, $v IN $map {
                  ECHO "$k=$v"
                }
            "#},
                },
                Example {
                    name: "expand every match",
                    fence_meta: None,
                    code: indoc! {r#"
                # single-line body; $x is a template path, WHO an override
                WRITE a.txt "hi \{{ env:WHO }}!"
                FOR $x IN GLOB("*.txt") { EXPAND $x WHO=World }
                ASSERT_STDOUT "hi World!"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "IF",
            syntax: "IF <expr> { <commands> } [ELSE IF <expr> { <commands> }] [ELSE { <commands> }]",
            summary: "Conditional execution.",
            description: "The condition is evaluated as a boolean expression. Prefix `!` negates (`IF !false`); only Bool values are accepted as conditions.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "if else",
                fence_meta: None,
                code: indoc! {r#"
                IF true {
                  ECHO yes
                } ELSE {
                  ECHO no
                }

                IF false {
                  ECHO skipped
                } ELSE IF true {
                  ECHO fallback
                }

                IF !false {
                  ECHO inverted
                }
            "#},
            }],
        },
        CommandMeta {
            name: "LET",
            syntax: "LET $var = <expr> | LET $var = ASYNC { <commands> }",
            summary: "Bind script-local variables.",
            description: "Assigns a value to a script-local variable. Variables are usable in templates (`{{ $var }}`), guards, and expressions. With `ASYNC`, spawns a background task and stores its handle (see ASYNC). The `$` sigil on the name is mandatory. The right-hand side is always an expression — literals, lists, maps, comparisons, `GLOB(\"*.md\")` — never a `{{ ... }}` template; interpolation happens in string values, not here. Bare words need no quotes: `LET $d = 30s` binds the same string as `LET $d = \"30s\"`.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "let",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $name = "world"
                ECHO "hello, {{ $name }}"

                LET $items = ["a", "b"]
                LET $count = 42
            "#},
                },
                Example {
                    name: "glob binding",
                    fence_meta: None,
                    code: indoc! {r#"
                # the RHS is an expression: GLOB(...) runs and binds a list
                WRITE a.txt "x"
                LET $files = GLOB("*.txt")
                FOR $f IN $files { ECHO $f }
                ASSERT_STDOUT "a.txt"
            "#},
                },
                Example {
                    name: "scoped variable reverts",
                    fence_meta: None,
                    code: indoc! {r#"
                # LET inside a braced block reverts when the block exits
                LET $a = "outer"
                [bool:true] {
                    LET $a = "inner"
                    WRITE inner.txt "{{ $a }}"
                }
                WRITE outer.txt "{{ $a }}"
                ASSERT_FILE inner.txt "inner"
                ASSERT_FILE outer.txt "outer"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "ASYNC",
            syntax: "ASYNC <command...> | ASYNC { <commands> } | LET $var = ASYNC { <commands> }",
            summary: "Run steps in a background thread.",
            description: "Runs a command or block of commands in a background thread with subshell isolation. Mutations (ENV, WORKDIR) stay within the block. With `LET`, stores a task handle for `AWAIT`.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "async",
                    fence_meta: None,
                    code: indoc! {r#"
                    ASYNC ECHO "first"

                    ASYNC {
                        ECHO "first"
                        ECHO "second"
                    }
                "#},
                },
                Example {
                    name: "async task handle",
                    fence_meta: None,
                    code: indoc! {r#"
                    LET $task = ASYNC {
                        ECHO "built"
                    }
                    AWAIT $task
                "#},
                },
            ],
        },
        CommandMeta {
            name: "AWAIT",
            syntax: "AWAIT $var",
            summary: "Join a background task.",
            description: "Blocks until the named task completes. Propagates errors if the task failed.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "await",
                fence_meta: None,
                code: indoc! {r#"
                LET $task = ASYNC ECHO "done"
                AWAIT $task
            "#},
            }],
        },
        CommandMeta {
            name: "CANCEL",
            syntax: "CANCEL $var",
            summary: "Synchronously cancel a background task.",
            description: "Kills the named background task spawned via LET $var = ASYNC .... Blocking: returns only after the task thread has been joined and its OS process reaped, so no residual filesystem or stream mutation follows. A later AWAIT $var reports cancellation. Only named tasks can be cancelled.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "cancel",
                fence_meta: None,
                code: indoc! {r#"
                LET $task = ASYNC SLEEP 30s
                CANCEL $task
            "#},
            }],
        },
        CommandMeta {
            name: "TIMEOUT",
            syntax: "TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> } | TIMEOUT <duration> AWAIT $var",
            summary: "Enforce an execution deadline.",
            description: "Aborts the wrapped step or block with a deadline error if it exceeds the duration (e.g. 500ms, 10s, 2m; a bare number means seconds). A blocking foreground process is killed.",
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "timeout",
                    fence_meta: None,
                    code: indoc! {r#"TIMEOUT 30s WRITE heartbeat.txt alive"#},
                },
                Example {
                    name: "timeout block",
                    fence_meta: None,
                    code: indoc! {r#"
                    TIMEOUT 30s {
                        WRITE a.txt one
                        WRITE b.txt two
                    }
                "#},
                },
                Example {
                    name: "timeout variable duration",
                    fence_meta: None,
                    code: indoc! {r#"
                    # durations resolve at runtime, so variables work too
                    LET $budget = "30s"
                    TIMEOUT $budget WRITE heartbeat.txt alive
                    ASSERT_FILE heartbeat.txt alive
                "#},
                },
            ],
        },
    ]
}

// ── Display ────────────────────────────────────────────────────────────────

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepKind::InheritEnv { keys } => write!(f, "INHERIT_ENV [{}]", keys.join(", ")),
            StepKind::Workdir(a) => write!(f, "WORKDIR {}", fmt_value(a, quote_arg)),
            StepKind::Workspace(t) => write!(f, "WORKSPACE {}", t),
            StepKind::Env { key, value } => {
                write!(f, "ENV {}={}", key, fmt_value(value, quote_arg))
            }
            StepKind::Run(c) => write!(f, "RUN {}", fmt_value(c, quote_run)),
            StepKind::RunExec { argv } => {
                let parts: Vec<String> = argv.iter().map(fmt_exec_arg).collect();
                write!(f, "RUN [{}]", parts.join(", "))
            }
            StepKind::Echo(m) => write!(f, "ECHO {}", fmt_value(m, quote_msg)),
            StepKind::Copy {
                from_current_workspace,
                from,
                to,
            } => {
                if *from_current_workspace {
                    write!(
                        f,
                        "COPY --from-current-workspace {} {}",
                        fmt_value(from, quote_arg),
                        fmt_value(to, quote_arg)
                    )
                } else {
                    write!(
                        f,
                        "COPY {} {}",
                        fmt_value(from, quote_arg),
                        fmt_value(to, quote_arg)
                    )
                }
            }
            StepKind::Symlink { from, to } => write!(
                f,
                "SYMLINK {} {}",
                fmt_value(from, quote_arg),
                fmt_value(to, quote_arg)
            ),
            StepKind::Mkdir(a) => write!(f, "MKDIR {}", fmt_value(a, quote_arg)),
            StepKind::Ls(a) => {
                write!(f, "LS")?;
                if let Some(x) = a {
                    write!(f, " {}", fmt_value(x, quote_arg))?;
                }
                Ok(())
            }
            StepKind::Cwd => write!(f, "CWD"),
            StepKind::Read(a) => {
                write!(f, "READ")?;
                if let Some(x) = a {
                    write!(f, " {}", fmt_value(x, quote_arg))?;
                }
                Ok(())
            }
            StepKind::ReadLine { var } => write!(f, "READ_LINE ${}", var),
            StepKind::Write { path, contents } => {
                write!(f, "WRITE {}", fmt_value(path, quote_arg))?;
                if let Some(b) = contents {
                    write!(f, " {}", fmt_value(b, quote_msg))?;
                }
                Ok(())
            }
            StepKind::Append { path, contents } => {
                write!(f, "APPEND {}", fmt_value(path, quote_arg))?;
                if let Some(b) = contents {
                    write!(f, " {}", fmt_value(b, quote_msg))?;
                }
                Ok(())
            }
            StepKind::Expand { path, overrides } => {
                write!(f, "EXPAND")?;
                if let Some(p) = path {
                    write!(f, " {}", fmt_value(p, quote_arg))?;
                }
                for (k, v) in overrides {
                    write!(f, " {}={}", k, fmt_value(v, quote_arg))?;
                }
                Ok(())
            }
            StepKind::AssertFile {
                hash,
                path,
                contents,
            } => {
                if let Some(d) = hash {
                    write!(f, "ASSERT_FILE --hash {} {}", d, fmt_value(path, quote_arg))
                } else {
                    write!(f, "ASSERT_FILE {}", fmt_value(path, quote_arg))?;
                    if let Some(b) = contents {
                        write!(f, " {}", fmt_value(b, quote_msg))?;
                    }
                    Ok(())
                }
            }
            StepKind::AssertDir(a) => write!(f, "ASSERT_DIR {}", fmt_value(a, quote_arg)),
            StepKind::AssertAbsent(a) => write!(f, "ASSERT_ABSENT {}", fmt_value(a, quote_arg)),
            StepKind::AssertStdout(m) => write!(f, "ASSERT_STDOUT {}", fmt_value(m, quote_msg)),
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
                        fmt_value(rev, quote_arg),
                        fmt_value(from, quote_arg),
                        fmt_value(to, quote_arg)
                    )
                } else {
                    write!(
                        f,
                        "COPY_GIT {} {} {}",
                        fmt_value(rev, quote_arg),
                        fmt_value(from, quote_arg),
                        fmt_value(to, quote_arg)
                    )
                }
            }
            StepKind::HashSha256 { path } => {
                write!(f, "HASH_SHA256 {}", fmt_value(path, quote_arg))
            }
            StepKind::Exit(code) => write!(f, "EXIT {}", fmt_raw_arg(code)),
            StepKind::Sleep { duration } => write!(f, "SLEEP {}", fmt_raw_arg(duration)),
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
            StepKind::AsyncBlock { body } => {
                write!(f, "ASYNC {{")?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::AssignAsync { var, body } => {
                write!(f, "LET ${} = ASYNC {{", var)?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::Await { var } => write!(f, "AWAIT ${}", var),
            StepKind::Cancel { var } => write!(f, "CANCEL ${}", var),
            StepKind::Timeout { duration, body } => {
                let budget = fmt_raw_arg(duration);
                if body.len() == 1 {
                    write!(f, "TIMEOUT {} {}", budget, body[0].kind)
                } else {
                    write!(f, "TIMEOUT {} {{", budget)?;
                    for s in body {
                        write!(f, "\n    {}", s)?;
                    }
                    write!(f, "\n}}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{format_duration, parse_duration};
    use crate::parser::parse_script;

    fn parse_err(script: &str) -> String {
        parse_script(script, lower_command)
            .expect_err("script must fail to parse")
            .to_string()
    }

    #[test]
    fn malformed_with_io_binding_names_the_bad_binding() {
        let err = parse_err("WITH_IO [stdout=discard] ECHO \"test\"\n");
        assert!(err.contains("invalid syntax for command WITH_IO"), "{err}");
        assert!(!err.contains("unknown command"), "{err}");
        assert!(err.contains("stdout=discard"), "{err}");
        assert!(err.contains("pipe:<name>"), "{err}");
    }

    #[test]
    fn await_without_task_variable_points_at_syntax() {
        let err = parse_err("AWAIT ECHO \"test\"\n");
        assert!(err.contains("invalid syntax for command AWAIT"), "{err}");
        assert!(!err.contains("unknown command"), "{err}");
        assert!(err.contains("AWAIT $t"), "{err}");
        assert!(err.contains("ECHO"), "{err}");
    }

    #[test]
    fn structural_fallthrough_commits_per_keyword() {
        for (script, cmd) in [
            ("CANCEL foo\n", "CANCEL"),
            ("TIMEOUT foo\n", "TIMEOUT"),
            ("FOR foo\n", "FOR"),
            ("IF foo\n", "IF"),
            ("LET foo\n", "LET"),
            // NOTE: INHERIT_ENV is dual-registered as a leaf command
            // (`INHERIT_ENV <key>...`), so `INHERIT_ENV foo` lowers
            // successfully instead of erroring — excluded here.
            ("ASYNC\n", "ASYNC"),
            ("ELSE foo\n", "ELSE"),
        ] {
            let err = parse_err(script);
            assert!(
                err.contains(&format!("invalid syntax for command {cmd}")),
                "{cmd}: {err}"
            );
            assert!(!err.contains("unknown command"), "{cmd}: {err}");
        }
    }

    #[test]
    fn leaf_arity_errors_carry_invalid_syntax_prefix() {
        let err = parse_err("SLEEP 1s 2s\n");
        assert!(err.contains("invalid syntax for command SLEEP"), "{err}");
        assert!(!err.contains("unknown command"), "{err}");
    }

    #[test]
    fn genuinely_unknown_command_keeps_bare_message() {
        let err = parse_err("FROBNICATE hi\n");
        assert!(err.contains("unknown command: FROBNICATE"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");
    }

    #[test]
    fn lowercase_command_suggests_uppercase() {
        // Lowercase never reaches lowering through `parse_script` (the
        // grammar rejects it with its own uppercase hint), so exercise the
        // public `lower_command` dispatcher directly.
        let err = lower_command("echo", vec![Arg::String("hi".to_string(), false)])
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("unknown command: echo"), "{err}");
        assert!(err.contains("did you mean `ECHO`"), "{err}");
    }

    #[test]
    fn parse_duration_units() {
        use std::time::Duration;
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("banana").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("-5s").is_err());
    }

    #[test]
    fn format_duration_round_trips() {
        for text in ["500ms", "10s", "2m", "1h", "90s", "1500ms"] {
            let parsed = parse_duration(text).unwrap();
            let rendered = format_duration(&parsed);
            assert_eq!(
                parse_duration(&rendered).unwrap(),
                parsed,
                "round-trip failed for {text}"
            );
        }
        assert_eq!(format_duration(&parse_duration("90s").unwrap()), "90s");
        assert_eq!(format_duration(&parse_duration("2m").unwrap()), "2m");
    }

    #[test]
    fn structural_metadata_covers_all_structural_kinds() {
        use crate::ast::Value;

        // Tripwire: adding a structural StepKind variant without registering
        // documentation fails to compile here (non-exhaustive match). Leaf
        // commands map to None; they are covered by declare_commands!.
        fn metadata_name(kind: &StepKind) -> Option<&'static str> {
            match kind {
                StepKind::WithIo { .. } | StepKind::WithIoBlock { .. } => Some("WITH_IO"),
                StepKind::For { .. } => Some("FOR"),
                StepKind::If { .. } => Some("IF"),
                StepKind::Assign { .. } => Some("LET"),
                StepKind::AsyncBlock { .. } | StepKind::AssignAsync { .. } => Some("ASYNC"),
                StepKind::Await { .. } => Some("AWAIT"),
                StepKind::Cancel { .. } => Some("CANCEL"),
                StepKind::Timeout { .. } => Some("TIMEOUT"),
                StepKind::RunExec { .. } => None,
                StepKind::Workdir(_)
                | StepKind::Workspace(_)
                | StepKind::Env { .. }
                | StepKind::InheritEnv { .. }
                | StepKind::Run(_)
                | StepKind::Echo(_)
                | StepKind::Copy { .. }
                | StepKind::Symlink { .. }
                | StepKind::Mkdir(_)
                | StepKind::Ls(_)
                | StepKind::Cwd
                | StepKind::Read(_)
                | StepKind::ReadLine { .. }
                | StepKind::Write { .. }
                | StepKind::Append { .. }
                | StepKind::Expand { .. }
                | StepKind::AssertFile { .. }
                | StepKind::AssertDir(_)
                | StepKind::AssertAbsent(_)
                | StepKind::AssertStdout(_)
                | StepKind::CopyGit { .. }
                | StepKind::HashSha256 { .. }
                | StepKind::Exit(_)
                | StepKind::Sleep { .. } => None,
            }
        }

        // Exercise the matcher once per structural variant so the arms cannot
        // rot (a new variant breaks compilation above first).
        let dummies: Vec<StepKind> = vec![
            StepKind::WithIo {
                bindings: Vec::new(),
                cmd: Box::new(StepKind::Echo(crate::ast::Arg::String(
                    "x".to_string(),
                    false,
                ))),
            },
            StepKind::For {
                key_var: None,
                var: "i".to_string(),
                in_expr: Expr::Literal(Value::Bool(true)),
                body: Vec::new(),
            },
            StepKind::If {
                cond: Box::new(Expr::Literal(Value::Bool(true))),
                then_body: Vec::new(),
                else_ifs: Vec::new(),
                else_body: None,
            },
            StepKind::Assign {
                var: "v".to_string(),
                expr: Expr::Literal(Value::Bool(true)),
            },
            StepKind::AsyncBlock { body: Vec::new() },
            StepKind::AssignAsync {
                var: "t".to_string(),
                body: Vec::new(),
            },
            StepKind::Await {
                var: "t".to_string(),
            },
            StepKind::Cancel {
                var: "t".to_string(),
            },
            StepKind::Timeout {
                duration: Arg::String("1s".to_string(), false),
                body: Vec::new(),
            },
        ];
        let registry = all_structural_metadata();
        for kind in &dummies {
            let name = metadata_name(kind).expect("structural kind must map to metadata");
            assert!(
                registry.iter().any(|meta| meta.name == name),
                "no structural metadata entry for {}",
                name
            );
        }
    }

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

            let matching = ast.iter().find(|step| {
                let kind = match &step.kind {
                    StepKind::WithIo { cmd, .. } => &**cmd,
                    other => other,
                };
                // Full Display covers wrapper kinds themselves (e.g. a
                // WithIo step displays as WITH_IO ...); unwrapped covers
                // wrapped leaf commands.
                kind.to_string().starts_with(meta.name)
                    || step.kind.to_string().starts_with(meta.name)
            });

            assert!(
                matching.is_some(),
                "No step in example for {} produces Display starting with {}",
                meta.name,
                meta.name
            );
        }
    }
}
