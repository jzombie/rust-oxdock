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

use crate::ast::{
    Arg, ArgPart, Expr, IoBinding, IoStream, PipeTarget, Step, Value, WorkspaceTarget,
};
use crate::command::{
    ArgSpec, ArgType, CommandMeta, Example, FlagSpec, FlagValueType, IoDirection, Stream,
    split_assignment,
};
use crate::constants::{KEYWORD_EXPORT, KEYWORD_IMPORT};
use crate::error::{ParseError, ParseResult, SpanContext};
use indoc::indoc;

// ── Helpers ────────────────────────────────────────────────────────────────

// Value-parsing helpers (`strip_surrounding_quotes`,
// `split_assignment`, `parse_duration`, `format_duration`) live in
// `crate::command` beside the `ArgType` validators that call them.

/// Join free-text tail arguments into one value. Single args pass through
/// untouched (preserving `Arg::Expr`); all-`String` tails join exactly like the
/// historical `join_args`; tails containing expressions become `Arg::Parts`
/// with single-space separators so `$x` is never silently dropped.
fn join_value(args: Vec<Arg>, cmd_name: &str) -> ParseResult<Arg> {
    if args.is_empty() {
        return Err(ParseError::validation(
            cmd_name,
            format!("{cmd_name} requires at least one argument"),
            &SpanContext::line_only(0),
        ));
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
pub fn lower_env_assignment(args: Vec<Arg>) -> ParseResult<StepKind> {
    let arg = args.into_iter().next().ok_or_else(|| {
        ParseError::validation(
            "ENV",
            "ENV requires KEY=value".to_string(),
            &SpanContext::line_only(0),
        )
    })?;
    let Some((key, value)) = split_assignment(arg.as_str())
        .map_err(|e| ParseError::validation("ENV", e.to_string(), &SpanContext::line_only(0)))?
    else {
        return Err(ParseError::validation(
            "ENV",
            "ENV requires KEY=value format".to_string(),
            &SpanContext::line_only(0),
        ));
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

/// Render an [`AssertTarget`] for `Display`: stream markers print bare
/// (`stdout` reparses to the marker); values print like other args.
fn fmt_assert_target(target: &AssertTarget) -> String {
    match target {
        AssertTarget::Value(arg) => fmt_value(arg, quote_msg),
        _ => target.render(),
    }
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
        && !crate::Command::is_statement_keyword(s);
    if is_safe && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn quote_msg(s: &str) -> String {
    let safe = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && !crate::Command::is_statement_keyword(s);
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
/// `F()`, ints, bools, nested lists) print raw via `render` so
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
    match &b.pipe {
        Some(PipeTarget::Name(p)) => format!("{}=pipe:{}", s, p),
        Some(PipeTarget::Var(v)) => format!("{}=${}", s, v),
        None => s.to_string(),
    }
}

// ── declare_commands! ──────────────────────────────────────────────────────

// Keywords parsed by PEG rules rather than plain-command lowering (`WITH_IO`,
// `AWAIT`, ...). When a line starts with one of these but fails to parse as
// such, lowering falls through here — report a committed syntax error instead
// of an unknown command.
pub(crate) fn is_known_command(name: &str) -> bool {
    if name == "ELSE" {
        return true;
    }
    all_metadata().iter().any(|meta| meta.name == name)
}

pub(crate) fn invalid_syntax_error(name: &str, raw_args: &[Arg]) -> ParseError {
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
    let found = if received.is_empty() {
        None
    } else {
        Some(received.clone())
    };
    let expected = all_metadata()
        .iter()
        .find(|meta| meta.name == name)
        .map(|meta| vec![meta.syntax.to_string()])
        .unwrap_or_default();
    let ctx = SpanContext::line_only(0);
    match structural_hint(name, &received) {
        Some(hint) => ParseError::invalid_syntax(
            name,
            format!("invalid syntax for command {name}: {hint}"),
            found,
            expected,
            Some(hint),
            &ctx,
        ),
        None => ParseError::invalid_syntax(
            name,
            format!("invalid syntax for command {name}: got {got}."),
            found,
            expected,
            None,
            &ctx,
        ),
    }
}

fn unknown_command_error(name: &str, raw_args: &[Arg]) -> ParseError {
    let received = raw_args
        .iter()
        .map(Arg::render)
        .collect::<Vec<_>>()
        .join(" ");
    let hint = structural_hint(name, &received).or_else(|| case_hint(name));
    let ctx = SpanContext::line_only(0);
    match hint {
        Some(hint) => ParseError::unknown_command(
            name,
            format!("unknown command: {name}\n{hint}"),
            Some(hint),
            &ctx,
        ),
        None => ParseError::unknown_command(name, format!("unknown command: {name}"), None, &ctx),
    }
}

/// Single decision function for the lowering fallback: keyword led lines
/// (structural statements, `ELSE`, every registered command) are committed
/// syntax errors, never unknown commands. Only truly unknown names fall
/// through to `unknown_command_error`. Callers enrich the result with the
/// token span via `ParseError::with_span`.
pub(crate) fn classify(name: &str, raw_args: &[Arg]) -> ParseError {
    if is_known_command(name) {
        invalid_syntax_error(name, raw_args)
    } else {
        unknown_command_error(name, raw_args)
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
            "AWAIT waits for a background task variable, e.g. `LET $t: HANDLE = ASYNC ECHO hi` then `AWAIT $t`; got {got}."
        )),
        "CANCEL" => Some(format!(
            "CANCEL stops a background task variable, e.g. `CANCEL $t` (from `LET $t: HANDLE = ASYNC ...`); got {got}."
        )),
        "ASYNC" => Some(format!(
            "ASYNC runs a command in the background, e.g. `ASYNC RUN ...`, `ASYNC {{ ... }}`, or `LET $t: HANDLE = ASYNC ...`; got {got}."
        )),
        "FOR" => Some(format!(
            "FOR loops need `FOR $item: TYPE IN <expr> {{ ... }}` (or `FOR $key: STRING, $value: TYPE IN <expr> {{ ... }}`); got {got}."
        )),
        "IF" => Some(format!(
            "IF needs a condition and a block, e.g. `IF true {{ ECHO yes }}`; got {got}."
        )),
        "ELSE" => Some(format!(
            "ELSE must directly follow an `IF ... {{ ... }}` block, e.g. `IF true {{ ECHO yes }} ELSE {{ ECHO no }}`; got {got}."
        )),
        "LET" => Some(format!(
            "LET assigns a variable, e.g. `LET $name: STRING = <expr>`, `LET $t: HANDLE = ASYNC ...`, `LET $out: STRING = <command>` (capture), or `LET $out: STRING = AWAIT $t`; got {got}."
        )),
        "SET" => Some(
            "`SET` is not a keyword; mutate a declared variable with `$var = <expr>`, e.g. `$count = 2`.".to_string(),
        ),
        "TIMEOUT" => Some(format!(
            "TIMEOUT needs a duration and a command or block, e.g. `TIMEOUT 30s RUN ...`; got {got}."
        )),
        "FUNC" => Some(format!(
            "FUNC defines a function, e.g. `FUNC GREET($name: STRING) {{ RETURN $name }}`; got {got}."
        )),
        "RETURN" => Some(format!(
            "RETURN ends a function with a value, e.g. `RETURN $x`; got {got}."
        )),
        "WHILE" => Some(format!(
            "WHILE needs a Bool condition and a block, e.g. `WHILE !$done {{ ... }}`; got {got}."
        )),
        "BREAK" => Some(
            "`BREAK` exits the innermost enclosing FOR/WHILE loop; it must appear inside a loop.".to_string(),
        ),
        "CONTINUE" => Some(
            "`CONTINUE` skips to the next iteration of the innermost enclosing FOR/WHILE loop; it must appear inside a loop.".to_string(),
        ),
        "INHERIT_ENV" => Some(format!(
            "INHERIT_ENV takes a key list, e.g. `INHERIT_ENV [HOME, PATH]`; got {got}."
        )),
        name if name == KEYWORD_IMPORT => Some(format!(
            "IMPORT brings module functions into bare-call scope, e.g. `IMPORT [STD]` or `IMPORT [STD, MOCK]`; got {got}."
        )),
        name if name == KEYWORD_EXPORT => Some(
            "`EXPORT` is reserved for future script-module support and cannot be used yet."
                .to_string(),
        ),
        _ => None,
    }
}

/// Diagnose a `WITH_IO` line that failed to parse: most often a malformed
/// binding list (bindings are bare streams or `<stream>=pipe:<name>`).
fn with_io_hint(got: &str, received: &str) -> String {
    const SYNTAX: &str =
        "WITH_IO needs `WITH_IO [bindings] <command>` or `WITH_IO [bindings] { <commands> }`";
    const BINDINGS: &str = "bindings are `stdin`, `stdout`, `stderr`, `<stream>=pipe:<name>`, or `<stream>=$var` with a PIPE-typed variable (e.g. `[stdout=pipe:log]`, `[stdin=$p]`)";
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
        #[derive(Debug, Clone, PartialEq)]
        pub enum StepKind {
            $( $vname $( { $( $vfname : $vftype ),* } )? $( ( $( $ttuple ),* ) )?, )*
            $( $sname $( { $( $sfname : $sftype ),* } )?, )*
        }

        pub fn lower_command(name: &str, raw_args: Vec<Arg>) -> ParseResult<StepKind> {
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
                        let lower_fn: fn(Vec<(String, Arg)>, Vec<Arg>) -> ParseResult<StepKind> = $lower;
                        lower_fn(flags, positional)
                    }
                )*
                _ => {
                    Err(classify(name, &raw_args))
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

/// First-argument target for `ASSERT_EQ` / `ASSERT_CONTAINS`.
///
/// Values (`Arg`) evaluate in memory and never touch disk. The `Stdout`,
/// `Stderr`, and `Pipe` markers observe stream buffers. Bare `stdout` /
/// `stderr` / `pipe:NAME` spellings lower to markers; quoted spellings stay
/// literal string values, so quoting remains interchangeable everywhere.
#[derive(Debug, Clone, PartialEq)]
pub enum AssertTarget {
    Value(Arg),
    Stdout,
    Stderr,
    Pipe(String),
}

impl AssertTarget {
    pub fn render(&self) -> String {
        match self {
            AssertTarget::Value(arg) => arg.render(),
            AssertTarget::Stdout => "stdout".to_string(),
            AssertTarget::Stderr => "stderr".to_string(),
            AssertTarget::Pipe(name) => format!("pipe:{name}"),
        }
    }
}

/// Lower the first positional of `ASSERT_EQ` / `ASSERT_CONTAINS`.
///
/// `Arg::Expr` (variables, key-paths, calls) is always a value. Bare
/// (unquoted) `stdout` / `stderr` / `pipe:NAME` spellings become stream
/// markers; every other spelling, quoted or not, stays a literal value.
/// In particular a `$var` holding a path never reads disk, and quoted
/// `"stdout"` names the seven-character string, not the stream.
fn lower_assert_target(arg: Arg, cmd_name: &str) -> ParseResult<AssertTarget> {
    match arg {
        Arg::Expr(_) => Ok(AssertTarget::Value(arg)),
        Arg::String(text, quoted) if !quoted => match text.as_str() {
            "stdout" => Ok(AssertTarget::Stdout),
            "stderr" => Ok(AssertTarget::Stderr),
            _ => match text.strip_prefix("pipe:") {
                Some(name) if !name.is_empty() => Ok(AssertTarget::Pipe(name.to_string())),
                Some(_) => Err(ParseError::validation(
                    cmd_name,
                    format!("{cmd_name} pipe target needs a name, got {text:?}"),
                    &SpanContext::line_only(0),
                )),
                None => Ok(AssertTarget::Value(lower_assert_operand(Arg::String(
                    text, false,
                )))),
            },
        },
        other => Ok(AssertTarget::Value(lower_assert_operand(other))),
    }
}

/// Give bare (unquoted, template-free) assertion operands the same typing
/// they carry in expression positions, so `ASSERT_EQ $status 200` compares
/// `Int(200)` rather than the string `"200"`. Signed integers (`-5` in
/// first position), decimals (`3.5`), and `true`/`false` all convert;
/// everything else, including quoted strings, stays a string. Note a
/// grammar property, not a limitation of this helper: `$x -5` in argument
/// position parses as subtraction (`expr_add_sub`), so negative expected
/// values must be bound first (`LET $e: INT = 0 - 5`).
fn lower_assert_operand(arg: Arg) -> Arg {
    match arg {
        Arg::String(text, false) => {
            if let Ok(i) = text.parse::<i64>() {
                Arg::Expr(Expr::Literal(Value::int(i)))
            } else if text.contains('.') && text.parse::<f64>().is_ok() {
                Arg::Expr(Expr::Literal(Value::float(
                    text.parse::<f64>().unwrap_or(f64::NAN),
                )))
            } else if text == "true" {
                Arg::Expr(Expr::Literal(Value::bool(true)))
            } else if text == "false" {
                Arg::Expr(Expr::Literal(Value::bool(false)))
            } else {
                Arg::String(text, false)
            }
        }
        other => other,
    }
}

declare_commands! {
    structural [
        WithIo { bindings: Vec<IoBinding>, cmd: Box<StepKind> },
        WithIoBlock { bindings: Vec<IoBinding> },
        For { key_var: Option<String>, key_type: Option<String>, var: String, var_type: String, in_expr: Expr, body: Vec<Step> },
        If { cond: Box<Expr>, then_body: Vec<Step>, else_ifs: Vec<(Box<Expr>, Vec<Step>)>, else_body: Option<Vec<Step>> },
        Assign { var: String, decl_type: String, expr: Expr },
        Set { var: String, expr: Expr },
        AssignCapture { var: String, decl_type: String, cmd: Box<StepKind> },
        AwaitCapture { out_var: String, out_type: String, task_var: String },
        AsyncBlock { body: Vec<Step> },
        AssignAsync { var: String, decl_type: String, body: Vec<Step> },
        Await { var: String },
        Cancel { var: String },
        Timeout { duration: Arg, body: Vec<Step> },
        RunExec { argv: Vec<Arg> },
        FuncDef { name: String, params: Vec<(String, String)>, body: Vec<Step> },
        Call { name: String, args: Vec<Expr> },
        Return { expr: Box<Expr> },
        While { cond: Box<Expr>, body: Vec<Step> },
        Break,
        Continue,
    ]

    Workdir => [
        name: "WORKDIR",
        variant: Workdir(Arg),
        syntax: "WORKDIR <path>",
        summary: "Change the working directory.",
        description: indoc! {r#"
            Sets the current working directory.

            Relative paths resolve against the current directory; `/` resets to
            the workspace root. Paths cannot escape the workspace.
        "#},
        args: &[ ArgSpec { name: "path", arg_type: ArgType::Path, description: "Directory to change to", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "change working directory", fence_meta: None, code: indoc! {r#"
            WORKDIR project/src
            WRITE generated.txt generated-under-workdir
            LET $body: STRING = READ generated.txt
            ASSERT_EQ $body "generated-under-workdir"
        "#} } ],
        lower: |_flags, args| {
            let path = args.into_iter().next().ok_or_else(|| ParseError::validation("WORKDIR", "WORKDIR requires a path".to_string(), &SpanContext::line_only(0)))?;
            Ok(StepKind::Workdir(path))
        },
    ],

    Workspace => [
        name: "WORKSPACE",
        variant: Workspace(WorkspaceTarget),
        syntax: "WORKSPACE (SNAPSHOT|LOCAL, case-insensitive)",
        summary: "Switch workspace roots.",
        description: "SNAPSHOT or LOCAL root.",
        args: &[ ArgSpec { name: "target", arg_type: ArgType::OneOf(&["SNAPSHOT", "LOCAL"]), description: "Target root", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "switch roots", fence_meta: None, code: indoc! {r#"WORKSPACE LOCAL"#} } ],
        lower: |_flags, args| {
            let target = args.into_iter().next().ok_or_else(|| ParseError::validation("WORKSPACE", "WORKSPACE requires a target".to_string(), &SpanContext::line_only(0)))?;
            match target.as_str() {
                "SNAPSHOT" | "snapshot" => Ok(StepKind::Workspace(WorkspaceTarget::Snapshot)),
                "LOCAL" | "local" => Ok(StepKind::Workspace(WorkspaceTarget::Local)),
                other => Err(ParseError::validation("WORKSPACE", format!("unknown workspace target: {other}"), &SpanContext::line_only(0))),
            }
        },
    ],

    Env => [
        name: "ENV",
        variant: Env { key: String, value: Arg },
        syntax: "ENV KEY=value",
        summary: "Set an environment variable.",
        description: indoc! {r#"
            Inserts or updates an env var.

            The value uses the unified string-value rules shared by every command:
            `"..."` or `'...'` quotes keep exact bytes (spaces, tabs), a lone `$var`
            evaluates that variable, `{{ ... }}` placeholders interpolate, unquoted
            words join with single spaces, and the first `=` splits key from value
            (`KEY=a=b` stores `a=b`).

            A `$var` inside larger text stays literal — write `{{ $var }}` to
            interpolate there.
        "#},
        args: &[ ArgSpec { name: "assignment", arg_type: ArgType::KeyValue, description: "KEY=value pair; the value resolves as STRING", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[
            Example { name: "set env", fence_meta: None, code: indoc! {r#"ENV APP_MODE=production"#} },
            Example { name: "quoted value with spaces", fence_meta: None, code: indoc! {r#"
                # quotes keep the space: SET_FORTH stores `outer scope`
                ENV SET_FORTH="outer scope"
                WRITE out.txt "{{ env:SET_FORTH }}"
                LET $body: STRING = READ out.txt
                ASSERT_EQ $body "outer scope"
            "#} },
            Example { name: "variable value", fence_meta: None, code: indoc! {r#"
                # a lone $var evaluates, like ECHO $var
                LET $who: STRING = "Alice"
                ENV GREETING=$who
                WRITE out.txt "{{ env:GREETING }}"
                LET $body: STRING = READ out.txt
                ASSERT_EQ $body "Alice"
            "#} },
            Example { name: "all value forms agree", fence_meta: None, code: indoc! {r#"
                # a bare variable, a quoted literal, and a template all
                # store plain strings through the same value rules
                LET $x: STRING = "Ada"
                ENV A=$x
                ENV B="hello world"
                ENV C="{{ $x }} concatenated"
                WRITE check.txt "{{ env:A }}|{{ env:B }}|{{ env:C }}"
                LET $body: STRING = READ check.txt
                ASSERT_EQ $body "Ada|hello world|Ada concatenated"
            "#} },
            Example { name: "scoped env reverts", fence_meta: None, code: indoc! {r#"
                # ENV inside a braced block reverts when the block exits
                ENV MODE=production
                [bool:true] {
                    ENV MODE=staging
                    WRITE inner.txt "{{ env:MODE }}"
                }
                WRITE outer.txt "{{ env:MODE }}"
                LET $inner_body: STRING = READ inner.txt
                LET $outer_body: STRING = READ outer.txt
                ASSERT_EQ $inner_body "staging"
                ASSERT_EQ $outer_body "production"
            "#} },
        ],
        lower: |_flags, args| lower_env_assignment(args),
    ],

    InheritEnv => [
        name: "INHERIT_ENV",
        variant: InheritEnv { keys: Vec<String> },
        syntax: "INHERIT_ENV [<key>, ...]",
        summary: "Inherit env vars from host.",
        description: indoc! {r#"
            Declares which host environment variables to inherit into the script.

            Must appear before any other commands and at most once. Without this
            directive, the script starts with an empty environment.
        "#},
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
                LET $x: STRING = "World"
                ECHO {{ $x }}
                ECHO $x
                ASSERT_CONTAINS stdout "World"
            "#} },
        ],
        lower: |_flags, args| Ok(StepKind::Echo(join_value(args, "ECHO")?)),
    ],

    Run => [
        name: "RUN",
        variant: Run(Arg),
        syntax: "RUN <command...> | RUN [\"exe\", \"arg\", ...]",
        summary: "Execute shell command or direct executable.",
        description: indoc! {r#"
            Shell form (`RUN <command...>`) runs the joined command string in the
            system shell (`$SHELL -c` / `COMSPEC /C`).

            Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly
            with no shell, so there is no shell expansion, globbing, redirection,
            or pipes; use it for portable commands.

            Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`, `LET`) apply to
            both forms.
        "#},
        args: &[ ArgSpec { name: "command", arg_type: ArgType::Rest(&ArgType::String), description: "Command", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "run", fence_meta: None, code: indoc! {r#"RUN echo hello"#} }, Example { name: "run exec form", fence_meta: None, code: indoc! {r#"RUN ["cargo", "--version"]"#} } ],
        lower: |_flags, args| match args.as_slice() {
            [Arg::Expr(Expr::List(elems))] if elems.is_empty() => {
                Err(ParseError::validation("RUN", "RUN requires at least one argument".to_string(), &SpanContext::line_only(0)))
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
            LET $body: STRING = READ dst.txt
            ASSERT_EQ $body "content"
        "#} }, Example { name: "copy from workspace", fence_meta: Some("roots:unified"), code: indoc! {r#"
            WRITE ws-src.txt ws-content
            COPY --from-current-workspace ws-src.txt ws-copy.txt
            LET $body: STRING = READ ws-copy.txt
            ASSERT_EQ $body "ws-content"
        "#} } ],
        lower: |flags, args| {
            let from_current_workspace = flags.iter().any(|(k, _)| k == "from_current_workspace");
            let mut it = args.into_iter();
            let from = it.next().ok_or_else(|| ParseError::validation("COPY", "COPY requires a source".to_string(), &SpanContext::line_only(0)))?;
            let to = it.next().ok_or_else(|| ParseError::validation("COPY", "COPY requires a destination".to_string(), &SpanContext::line_only(0)))?;
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
            let rev = it.next().ok_or_else(|| ParseError::validation("COPY_GIT", "COPY_GIT requires a revision".to_string(), &SpanContext::line_only(0)))?;
            let from = it.next().ok_or_else(|| ParseError::validation("COPY_GIT", "COPY_GIT requires a source".to_string(), &SpanContext::line_only(0)))?;
            let to = it.next().ok_or_else(|| ParseError::validation("COPY_GIT", "COPY_GIT requires a destination".to_string(), &SpanContext::line_only(0)))?;
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
            LET $body: STRING = READ link.txt
            ASSERT_EQ $body "content"
        "#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let from = it.next().ok_or_else(|| ParseError::validation("SYMLINK", "SYMLINK requires a source".to_string(), &SpanContext::line_only(0)))?;
            let to = it.next().ok_or_else(|| ParseError::validation("SYMLINK", "SYMLINK requires a target".to_string(), &SpanContext::line_only(0)))?;
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
        lower: |_flags, args| Ok(StepKind::Mkdir(args.into_iter().next().ok_or_else(|| ParseError::validation("MKDIR", "MKDIR requires a path".to_string(), &SpanContext::line_only(0)))?)),
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
        description: indoc! {r#"
            Reads bytes until newline without waiting for EOF, leaving the pipe open.

            Trailing newline is stripped (shell-read parity). On premature EOF
            assigns accumulated bytes and returns.
        "#},
        args: &[ ArgSpec { name: "var", arg_type: ArgType::Var, description: "Target variable (`$name`); the line binds as STRING", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "read line", fence_meta: None, code: indoc! {r#"
            WITH_IO [stdout=pipe:lines] ECHO "first"
            WITH_IO [stdin=pipe:lines] READ_LINE $reply
        "#} } ],
        lower: |_flags, args| {
            let arg = args.into_iter().next().ok_or_else(|| ParseError::validation("READ_LINE", "READ_LINE requires a variable".to_string(), &SpanContext::line_only(0)))?;
            let var = match arg {
                Arg::Expr(Expr::Var(name)) => name,
                Arg::String(s, _) => s.trim_start_matches('$').to_string(),
                other => return Err(ParseError::validation("READ_LINE", format!("READ_LINE requires a $variable, found {:?}", other), &SpanContext::line_only(0))),
            };
            if var.is_empty() {
                return Err(ParseError::validation("READ_LINE", "READ_LINE requires a variable".to_string(), &SpanContext::line_only(0)))
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
            let path = it.next().ok_or_else(|| ParseError::validation("WRITE", "WRITE requires a path".to_string(), &SpanContext::line_only(0)))?;
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
            LET $all: STRING = READ log.txt
            ASSERT_EQ $all "line1line2"
        "#} } ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let path = it.next().ok_or_else(|| ParseError::validation("APPEND", "APPEND requires a path".to_string(), &SpanContext::line_only(0)))?;
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
        description: indoc! {r#"
            A template is any text file — or piped stdin when no path is given —
            containing `{{ ... }}` placeholders. EXPAND replaces each placeholder
            and prints the result to stdout.

            Placeholders: `{{ NAME }}` reads a `KEY=val` override passed on this
            command; `{{ env:NAME }}` reads an override, falling back to the
            environment; `{{ $var }}` reads a script variable (dotted paths allowed).
            A missing key is an error, never a silent empty.

            Substitution runs in a single pass. EXPAND is not recursive and does not
            expand nested placeholders: a value that itself contains `{{ ... }}` is
            inserted verbatim and never expanded again.

            A bare `$var` argument is a template path; `KEY=val` arguments are
            overrides whose values follow the unified string-value rules (same as
            `ENV`: quotes keep exact bytes, a lone `$var` evaluates,
            `{{ ... }}` interpolates).

            NOTE: `WRITE` interpolates `{{ ... }}` while writing, so escape it
            (`\{{ ... }}`) when writing a template file for a later `EXPAND`.

            With no path, the template arrives on stdin through a pipe. When piping
            from a shell, single-quote the template (`echo '{{ $x }}'`): double
            quotes let the shell swallow `$x`, so oxdock receives an empty `{{ }}`
            placeholder and errors.
        "#},
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
                ASSERT_CONTAINS stdout "Hello Alice!"
            "#} },
            Example { name: "override with spaces", fence_meta: None, code: indoc! {r#"
                # WRITE would interpolate {{ }} right away, so escape it:
                # the file must literally contain {{ env:NAME }} for EXPAND
                WRITE template.md "Hello \{{ env:NAME }}!"
                EXPAND template.md NAME="Alice Smith"
                ASSERT_CONTAINS stdout "Hello Alice Smith!"
            "#} },
            Example { name: "variable override", fence_meta: None, code: indoc! {r#"
                # same escaping: keep the placeholder literal until EXPAND;
                # a lone $who evaluates, like ECHO $who
                LET $who: STRING = "Bob"
                WRITE template.md "Hi \{{ env:WHO }}!"
                EXPAND template.md WHO=$who
                ASSERT_CONTAINS stdout "Hi Bob!"
            "#} },
            Example { name: "override forms agree", fence_meta: None, code: indoc! {r#"
                # a bare variable and a template-with-tail expand identically
                LET $x: STRING = "Ada"
                WRITE template.md "Hi \{{ env:NAME }} and \{{ env:NAME2 }}!"
                EXPAND template.md NAME=$x NAME2="{{ $x }} concatenated"
                ASSERT_CONTAINS stdout "Hi Ada and Ada concatenated!"
            "#} },
            Example { name: "expand stdin", fence_meta: None, code: indoc! {r#"
                # no path: the template arrives on stdin through a pipe
                WITH_IO [stdout=pipe:tpl] ECHO "Hello \{{ env:NAME }}!"
                WITH_IO [stdin=pipe:tpl] EXPAND NAME=Alice
                ASSERT_CONTAINS stdout "Hello Alice!"
            "#} },
            Example { name: "override does not leak", fence_meta: None, code: indoc! {r#"
                # KEY=val overrides shadow env for that EXPAND only —
                # they never update the environment itself
                ENV NAME="Alice"
                WRITE template.md "Hi \{{ env:NAME }}!"
                EXPAND template.md NAME="Bob"
                ASSERT_CONTAINS stdout "Hi Bob!"
                EXPAND template.md
                ASSERT_CONTAINS stdout "Hi Alice!"
            "#} },
        ],
        lower: |_flags, args| {
            let mut path = None;
            let mut overrides = Vec::new();
            for arg in args {
                let text = arg.as_str();
                if let Some((key, value)) = split_assignment(text).map_err(|e| ParseError::validation("EXPAND", e.to_string(), &SpanContext::line_only(0)))? {
                    overrides.push((key, value));
                } else if path.is_none() { path = Some(arg); }
                else { return Err(ParseError::validation("EXPAND", "EXPAND accepts at most one path".to_string(), &SpanContext::line_only(0))) }
            }
            Ok(StepKind::Expand { path, overrides })
        },
    ],

    AssertEq => [
        name: "ASSERT_EQ",
        variant: AssertEq { hash: Option<String>, actual: AssertTarget, expected: Option<Arg> },
        syntax: "ASSERT_EQ <actual> <expected> | ASSERT_EQ --hash <sha256> <actual>",
        summary: "Assert strict equality.",
        description: indoc! {r#"
            Compares two evaluated values with typed equality (no coercion:
            `INT(42)` never equals `STRING("42")`), aborting the pipeline
            with a step-numbered error showing expected vs actual otherwise.

            Both sides are values: `$var`, literals, templates, and calls
            evaluate in memory and never touch disk. Read files explicitly
            first (`LET $text: STRING = READ "out.txt"`, then
            `ASSERT_EQ $text ...`).
            Bare `stdout` / `stderr` observe stream buffers; `pipe:NAME`
            observes a pipe buffer. `--hash` compares the SHA-256 of a
            string, pipe, or captured-stdout actual instead of the raw
            bytes (`stderr` is unsupported).
        "#},
        args: &[
            ArgSpec { name: "actual", arg_type: ArgType::Any, description: "Value, stdout, stderr, or pipe:NAME", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "expected", arg_type: ArgType::Rest(&ArgType::Any), description: "Expected (required unless --hash)", io: IoDirection::Read, index: 1, required: false, fallback_stream: None },
        ],
        flags: &[ FlagSpec { name: "hash", long: "--hash", value_type: FlagValueType::String, required: false, description: "SHA-256" } ],
        default_output: None,
        examples: &[ Example { name: "assert eq", fence_meta: None, code: indoc! {r#"
            LET $status: INT = 200
            ASSERT_EQ $status 200
        "#} },
        Example { name: "assert eq file", fence_meta: None, code: indoc! {r#"
            WRITE payload.bin stable-content
            LET $body: STRING = READ payload.bin
            ASSERT_EQ $body "stable-content"
        "#} },
        Example { name: "assert eq hash", fence_meta: None, code: indoc! {r#"
            # --hash compares the SHA-256 digest instead of raw bytes
            WRITE payload.bin stable-content
            LET $body: STRING = READ payload.bin
            ASSERT_EQ --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c $body
        "#} } ],
        lower: |flags, args| {
            let hash = flags.iter().find(|(k, _)| k == "hash").map(|(_, v)| v.as_str().to_string());
            let mut it = args.into_iter();
            let actual = lower_assert_target(it.next().ok_or_else(|| ParseError::validation("ASSERT_EQ", "ASSERT_EQ requires a value".to_string(), &SpanContext::line_only(0)))?, "ASSERT_EQ")?;
            let remaining: Vec<Arg> = it
                .map(lower_assert_operand)
                .collect::<Vec<Arg>>();
            // Exactly two operands, except --hash carries its expectation
            // in the flag and takes none positionally.
            let expected = if remaining.is_empty() {
                if hash.is_some() {
                    None
                } else {
                    return Err(ParseError::validation("ASSERT_EQ", "ASSERT_EQ requires an expected value".to_string(), &SpanContext::line_only(0)))
                }
            } else {
                Some(join_value(remaining, "ASSERT_EQ")?)
            };
            Ok(StepKind::AssertEq { hash, actual, expected })
        },
    ],

    AssertContains => [
        name: "ASSERT_CONTAINS",
        variant: AssertContains { haystack: AssertTarget, needle: Arg },
        syntax: "ASSERT_CONTAINS <haystack> <needle>",
        summary: "Assert containment.",
        description: indoc! {r#"
            Checks containment and aborts the pipeline with a step-numbered
            error otherwise: substring for strings, element match for lists,
            key presence for maps, substring over stream and pipe buffers.

            Like `ASSERT_EQ`, both sides are values read without implicit
            I/O; read files explicitly first
            (`LET $text: STRING = READ "cfg.txt"`).
            Bare `stdout` / `stderr` observe stream buffers; `pipe:NAME`
            observes a pipe buffer.
        "#},
        args: &[
            ArgSpec { name: "haystack", arg_type: ArgType::Any, description: "Value, stdout, stderr, or pipe:NAME", io: IoDirection::Read, index: 0, required: true, fallback_stream: None },
            ArgSpec { name: "needle", arg_type: ArgType::Rest(&ArgType::Any), description: "Substring, element, or key", io: IoDirection::Read, index: 1, required: true, fallback_stream: None },
        ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "assert contains", fence_meta: None, code: indoc! {r#"
            ECHO build-complete
            ASSERT_CONTAINS stdout "build-complete"
        "#} } ],
        lower: |flags, args| {
            let _ = flags;
            let mut it = args.into_iter();
            let haystack = lower_assert_target(it.next().ok_or_else(|| ParseError::validation("ASSERT_CONTAINS", "ASSERT_CONTAINS requires a value".to_string(), &SpanContext::line_only(0)))?, "ASSERT_CONTAINS")?;
            let remaining: Vec<Arg> = it
                .map(lower_assert_operand)
                .collect::<Vec<Arg>>();
            if remaining.is_empty() {
                return Err(ParseError::validation("ASSERT_CONTAINS", "ASSERT_CONTAINS requires a needle".to_string(), &SpanContext::line_only(0)))
            }
            let needle = join_value(remaining, "ASSERT_CONTAINS")?;
            Ok(StepKind::AssertContains { haystack, needle })
        },
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
        lower: |_flags, args| Ok(StepKind::HashSha256 { path: args.into_iter().next().ok_or_else(|| ParseError::validation("HASH_SHA256", "HASH_SHA256 requires a path".to_string(), &SpanContext::line_only(0)))? }),
    ],

    Exit => [
        name: "EXIT",
        variant: Exit(Arg),
        syntax: "EXIT <code>",
        summary: "Exit pipeline.",
        description: indoc! {r#"
            Stops the pipeline immediately with an `EXIT requested with code <code>`
            error; steps after it never run, at any nesting depth.

            Enclosing blocks still unwind their LET/ENV/WORKDIR/WORKSPACE state,
            anonymous background tasks are killed synchronously, and files written
            before the EXIT persist.
        "#},
        args: &[ ArgSpec { name: "code", arg_type: ArgType::Int, description: "Code", io: IoDirection::Write, index: 0, required: true, fallback_stream: None } ],
        flags: &[],
        default_output: None,
        examples: &[ Example { name: "exit", fence_meta: Some("expect_error:\"EXIT requested with code 0\""), code: indoc! {r#"EXIT 0"#} } ],
        lower: |_flags, args| {
            // Static literals were already Int-checked by the central
            // validator; dynamics resolve (and validate) at runtime.
            let code = args.into_iter().next().ok_or_else(|| ParseError::validation("EXIT", "EXIT requires a code".to_string(), &SpanContext::line_only(0)))?;
            Ok(StepKind::Exit(code))
        },
    ],

    Sleep => [
        name: "SLEEP",
        variant: Sleep { duration: Arg },
        syntax: "SLEEP <duration>",
        summary: "Pause execution for a duration.",
        description: indoc! {r#"
            Parks the step for the duration (e.g. 500ms, 10s, 2m).

            Cooperative: checks for cancellation so an enclosing TIMEOUT or task
            teardown interrupts the sleep. Cross-platform alternative to shell sleep
            for testing time boundaries.
        "#},
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
                LET $pause: STRING = "100ms"
                SLEEP $pause
                LET $bare: STRING = 100ms
                SLEEP $bare
            "#},
            },
        ],
        lower: |_flags, args| {
            let mut it = args.into_iter();
            let raw = it
                .next()
                .ok_or_else(|| ParseError::validation("SLEEP", "SLEEP requires a duration (e.g. SLEEP 500ms)".to_string(), &SpanContext::line_only(0)))?;
            if it.next().is_some() {
                return Err(ParseError::validation("SLEEP", "SLEEP takes exactly one duration argument".to_string(), &SpanContext::line_only(0)))
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
            syntax: "WITH_IO [<stream>[=pipe:<name>|=$var], ...] <command> | WITH_IO [bindings] { <commands> }",
            summary: "Reroute standard streams.",
            description: indoc! {r#"
                Reroutes the standard streams of the next command or, in block form,
                of every enclosed command.

                Bindings map streams (`stdin`, `stdout`, `stderr`) to named script
                pipes (`stdout=pipe:name`, `stderr=pipe:name`) or to a PIPE-typed
                variable (`stdin=$p`, resolved against the live pipe registry when
                the step runs). Both stdout and stderr pipes capture output the same way.

                Pipes hold bytes in memory and spill to a temp file above 8 MiB, so a
                producer can finish before the consumer starts.

                If WITH_IO wraps an ASYNC block whose body is a single RUN, guarded or
                not, the pipe is a zero copy OS kernel pipe instead: pair it with a
                consumer that runs while the producer is alive, since output past the
                64 KiB kernel buffer stalls until drained. That promotion never crosses
                a function boundary: pipes created, bound, or passed by variable inside FUNC
                bodies are always script pipes, even when the surrounding task would
                otherwise promote.

                A second producer or consumer on a live name is an explicit error. A name
                bound as output can later feed another command's `stdin`, connecting
                commands without touching the terminal. Binding `stdout` and `stderr` to
                the same live pipe name fails deterministically. Merge streams in shell
                via `2>&1` instead.

                Nested blocks stack defaults; inline bindings override inherited ones for
                their command only; closing a block restores previous wiring.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "with_io block",
                    fence_meta: None,
                    code: indoc! {r#"
                WITH_IO [stdout=pipe:log] {
                  ECHO first
                  ECHO second
                }
                WITH_IO [stdin=pipe:log] WRITE captured.txt
            "#},
                },
                Example {
                    name: "variable pipe binding",
                    fence_meta: None,
                    code: indoc! {r#"
                # Declare the pipe first with the explicit handle operator
                # (like `env:KEY`): `pipe:log` names a pipe without touching
                # a stream. A plain string here would be a TypeMismatch.
                # `$p` (not `pipe:$p`) is the variable form; literals stay
                # `pipe:name`.
                LET $p: PIPE = pipe:log
                WITH_IO [stdout=$p] ECHO hello
                WITH_IO [stdin=$p] READ_LINE $line
                ASSERT_EQ $line "hello"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "FOR",
            syntax: "FOR $item: TYPE IN <expr> { <commands> } | FOR $key: STRING, $value: TYPE IN <expr> { <commands> }",
            summary: "Iterate over a list or map.",
            description: indoc! {r#"
                The loop variable receives each element (lists) or value (maps); with
                two variables, the first receives the key.

                Loop variables are declared with explicit types and scoped per iteration;
                they do not leak outward. The body may be a braced block
                or a single-line `{ ... }` command.

                `GLOB("...")` patterns must be quoted (`*` is not a bare word, so
                `GLOB(*)` is a parse error); GLOB returns a root-relative sorted list,
                empty when nothing matches, and rejects `..` escapes.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "for loop",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $items: LIST = ["a", "b"]
                FOR $item: STRING IN $items {
                  ECHO $item
                }

                LET $map: MAP = {"x": 1}
                FOR $k: STRING, $v: INT IN $map {
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
                IMPORT [STD]
                FOR $x: STRING IN GLOB("*.txt") { EXPAND $x WHO=World }
                ASSERT_CONTAINS stdout "hi World!"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "IF",
            syntax: "IF <expr> { <commands> } [ELSE IF <expr> { <commands> } ...] [ELSE { <commands> }]",
            summary: "Conditional execution.",
            description: indoc! {r#"
                The condition is evaluated as a boolean expression.

                Prefix `!` negates (`IF !false`); `&&` binds tighter than
                `||`, and both short-circuit, so `IF true || $missing`
                never evaluates the right side. Only Bool values are
                accepted as conditions.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "if else",
                    fence_meta: None,
                    code: indoc! {r#"
                IMPORT [STD]
                IF true {
                  WRITE yes.txt taken
                } ELSE {
                  WRITE yes.txt skipped
                }

                IF false {
                  WRITE skipped.txt no
                } ELSE IF true {
                  WRITE fallback.txt taken
                }

                # !false evaluates to true, so this branch runs.
                IF !false {
                  WRITE negated.txt taken
                }
                LET $yes_body: STRING = READ yes.txt
                LET $fallback_body: STRING = READ fallback.txt
                LET $negated_body: STRING = READ negated.txt
                ASSERT_EQ $yes_body "taken"
                ASSERT_EQ $fallback_body "taken"
                ASSERT_EQ $negated_body "taken"
                LET $t: STRING = PATH_TYPE("skipped.txt")
                ASSERT_EQ $t "absent"
            "#},
                },
                Example {
                    name: "logical condition composition",
                    fence_meta: None,
                    code: indoc! {r#"
                IMPORT [STD]
                LET $role: STRING = "admin"
                LET $level: INT = 3
                # || is true when either side holds; && needs both.
                IF $role == "owner" || $level >= 5 {
                    WRITE unexpected.txt no
                } ELSE {
                    WRITE fallback.txt or-false
                }
                IF $role == "admin" || $level >= 5 {
                    WRITE chosen.txt or-true
                }
                IF $role == "admin" && $level >= 5 {
                    WRITE unexpected-too.txt no
                } ELSE {
                    WRITE and.txt and-false
                }
                LET $fb: STRING = READ fallback.txt
                LET $ch: STRING = READ chosen.txt
                LET $an: STRING = READ and.txt
                ASSERT_EQ $fb "or-false"
                ASSERT_EQ $ch "or-true"
                ASSERT_EQ $an "and-false"
                LET $t1: STRING = PATH_TYPE("unexpected.txt")
                LET $t2: STRING = PATH_TYPE("unexpected-too.txt")
                ASSERT_EQ $t1 "absent"
                ASSERT_EQ $t2 "absent"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "LET",
            syntax: "LET $var: TYPE = <expr> | LET $var: TYPE = ASYNC { <commands> } | LET $var: TYPE = <command> | LET $var: TYPE = AWAIT $task",
            summary: "Bind script-local variables.",
            description: indoc! {r#"
                Declares a script-local variable with an explicit type (STRING, INT,
                FLOAT, BOOL, PIPE, LIST, MAP, HANDLE, DURATION, PATH). Duplicate LET
                in the same scope frame is a redeclaration error; mutate with
                `$var = <expr>`.

                Variables are usable in templates (`{{ $var }}`), guards, and
                expressions. With `ASYNC`, spawns a background task and stores its
                handle (see ASYNC). The `$` sigil on the name is mandatory.

                No hoisting: a variable exists only after its LET runs, in
                execution order. Reading `$var` before its LET (or after the
                block that declared it exits) fails with
                `undefined variable $var`. Scopes are a stack of frames and
                resolution walks innermost outward, so nothing pre-declares
                names. Function bodies read outer variables through the same
                walk, but their own LETs never leak out (see FUNC).

                The right-hand side is always an expression — literals, lists, maps,
                arithmetic (`+ - * /` with `*`/`/` binding tighter, unary `-`,
                parentheses), comparisons (`< <= > >=` binding tighter than
                `== !=`), logical `&&` (tighter) and `||` with short-circuit,
                `!` negation, `env:KEY` reads, `pipe:NAME` handles,
                `INSPECT($var)` snapshots, `GLOB("*.md")`, `INT(x)` /
                `FLOAT(x)` conversions — never a `{{ ... }}` template;
                interpolation happens in string values, not here.

                Numbers are numeric literals: `42` binds `INT`, `3.14` binds
                `FLOAT`. `Int x Int` stays `INT` (checked, integer division,
                so `7 / 2` is `3`); any `Float` operand promotes to `FLOAT`.
                Division by zero, overflow, and non-finite results are errors.
                Both numeric sides compare numerically (`1 == 1.0` is true);
                otherwise `==`/`!=` compare rendered strings and ordering on
                non-numerics is a Type Error. Constant subtrees fold at parse
                time and dynamic arithmetic compiles to flat RPN with
                identical semantics.

                Float equality is exact with no epsilon. Floats store decimals
                in binary, so a value is exact only when its reduced fraction
                has a power-of-2 denominator: 0.5 (1/2), 0.25 (1/4), 0.75
                (3/4) are exact, while 0.1 (1/10), 0.2 (1/5), 0.3 (3/10)
                repeat forever in binary (like 1/3 in decimal) and truncate,
                so `0.1 + 0.2 == 0.3` is false (the sum is
                `0.30000000000000004`). Rule of thumb: endings .5, .25, .75,
                .125, .625, .875 are exact; .1, .2, .3 and similar are
                approximations. Bound approximations instead of comparing
                them: `IF $sum > 0.299999 && $sum < 0.300001`.

                Comparisons do not chain: `a < b < c` is a parse error, not
                `(a < b) < c`. Chaining would compare a `BOOL` against a
                number (a runtime Type Error in C-style parsing) or evaluate
                the middle term twice (Python-style chaining), so the grammar
                accepts exactly one comparison operator per level. Write the
                conjunction explicitly: `$a < $b && $b < $c`. The same holds
                for equality (`$a == $b == $c` is rejected).

                Captured command output is a string, so convert before math:
                `LET $total: INT = $total + INT($size_str)` (`INT` trims ASCII
                whitespace; `FLOAT` accepts int strings and rejects
                non-finite).

                Bare words need no quotes: `LET $d: STRING = 30s` binds the same string
                as quoted.

                When the right-hand side is a synchronous command
                (`LET $out: STRING = ECHO hi`), the command runs to completion and its
                exact stdout bytes are captured into the variable as a string (no newline
                stripping; commands with no stdout capture as `""`; non-UTF8 stdout is
                an error). Combining capture with an explicit
                `WITH_IO [stdout=pipe:...]` is a parse error.

                Coming from Bash, the capture line looks familiar but behaves
                strictly:

                | | Bash `output=$(...)` | OxDock `LET $out: STRING = ...` |
                | --- | --- | --- |
                | Trailing newlines | Stripped (all of them) | Preserved byte-exact |
                | Variable type | Always an untyped string | Declared: STRING, INT, FLOAT, ... |
                | Math on output | Implicit: `$((var + 1))` | Explicit: `INT($out) + 1` |
                | Failing command | Continues with empty output unless `set -e` | Step fails immediately, binds nothing |

                `LET $out: STRING = AWAIT $var` captures a background task's stdout the
                same way; bare `AWAIT $var` forwards it to the parent stdout instead.

                `LET $e: STRING = env:FOO` reads the script environment into a plain
                string.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "let",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $name: STRING = "world"
                ECHO "hello, {{ $name }}"

                LET $items: LIST = ["a", "b"]
                LET $count: INT = 42
            "#},
                },
                Example {
                    name: "no hoisting",
                    fence_meta: Some("expect_error:\"undefined variable\""),
                    code: indoc! {r#"
                # reading before the LET runs is an error, not an empty value
                ECHO $too_early
                LET $too_early: STRING = "too late"
            "#},
                },
                Example {
                    name: "glob binding",
                    fence_meta: None,
                    code: indoc! {r#"
                # the RHS is an expression: GLOB(...) runs and binds a list
                WRITE a.txt "x"
                IMPORT [STD]
                LET $files: LIST = GLOB("*.txt")
                FOR $f: STRING IN $files { ECHO $f }
                ASSERT_CONTAINS stdout "a.txt"
            "#},
                },
                Example {
                    name: "scoped variable reverts",
                    fence_meta: None,
                    code: indoc! {r#"
                # LET inside a braced block reverts when the block exits
                LET $a: STRING = "outer"
                [bool:true] {
                    LET $a: STRING = "inner"
                    WRITE inner.txt "{{ $a }}"
                }
                WRITE outer.txt "{{ $a }}"
                LET $in_body: STRING = READ inner.txt
                LET $out_body: STRING = READ outer.txt
                ASSERT_EQ $in_body "inner"
                ASSERT_EQ $out_body "outer"
            "#},
                },
                Example {
                    name: "capture command output",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $out: STRING = ECHO hi
                ASSERT_EQ $out "hi\n"
            "#},
                },
                Example {
                    name: "arithmetic over captured output",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $size_str: STRING = ECHO 41
                IMPORT [STD]
                LET $total: INT = INT($size_str) + 1
                LET $ratio: FLOAT = 1 + 2.5
                # Int x Int stays INT: integer division truncates.
                LET $half: INT = 7 / 2
                ASSERT_EQ $total 42
                ASSERT_EQ $ratio 3.5
                ASSERT_EQ $half 3
            "#},
                },
                Example {
                    name: "float equality is exact",
                    fence_meta: None,
                    code: indoc! {r#"
                # Binary fractions compare cleanly; decimal fractions may not:
                # 0.1 + 0.2 is 0.30000000000000004, so == is false.
                IMPORT [STD]
                LET $exact: BOOL = 0.5 + 0.25 == 0.75
                LET $decimal: BOOL = 0.1 + 0.2 == 0.3
                IF $exact {
                    WRITE exact.txt yes
                }
                IF $decimal {
                    WRITE unexpected.txt no
                }
                LET $ok: STRING = READ exact.txt
                ASSERT_EQ $ok "yes"
                LET $t: STRING = PATH_TYPE("unexpected.txt")
                ASSERT_EQ $t "absent"
            "#},
                },
                Example {
                    name: "bound inexact decimals",
                    fence_meta: None,
                    code: indoc! {r#"
                # Never test inexact decimals for equality; bound them.
                LET $sum: FLOAT = 0.1 + 0.2
                IF $sum > 0.299999 && $sum < 0.300001 {
                    WRITE bounded.txt yes
                }
                LET $ok: STRING = READ bounded.txt
                ASSERT_EQ $ok "yes"
            "#},
                },
                Example {
                    name: "inspect a variable",
                    fence_meta: None,
                    code: indoc! {r#"
                # INSPECT($var) snapshots a variable into a MAP: declared
                # type plus live details (pipe backend stats here), so
                # scripts can branch on engine state.
                LET $p: PIPE = pipe:log
                WITH_IO [stdout=$p] ECHO hello
                LET $info: MAP = INSPECT($p)
                IF $info.is_os_pipe {
                    WRITE unexpected.txt "should be a script pipe"
                }
                ASSERT_EQ $info.type "PIPE"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "MUTATION",
            syntax: "$var = <expr>",
            summary: "Mutate a declared variable.",
            description: indoc! {r#"
                Reassigns an existing variable, converting the new value to
                the type declared at LET time. The explicit annotation is
                what authorizes string-to-number conversion here (`$n = "42"`
                binds 42 for an INT); a non-numeric string is an error.
                Expressions never convert: `"100" + 1` is a Type Error, use
                `INT()` / `FLOAT()` to cross that boundary explicitly.

                The leading `$` distinguishes mutation from `KEY=value` command
                assignments. Assigning an undeclared variable or a mismatched type is
                an error.

                Mutation writes through to the scope where the variable was
                declared, so it survives block exit: `LET $x` outside a block
                followed by `$x = ...` inside still reads back the new value
                afterwards, for every type. This is the counterpart to LET
                shadowing, where `LET $x` *inside* the block declares a
                separate inner variable that reverts on exit.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "mutate",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $count: INT = 1
                $count = 2
                ASSERT_EQ $count 2
            "#},
                },
                Example {
                    name: "convert before math",
                    fence_meta: None,
                    code: indoc! {r#"
                # Captured output is a string: `"100" + 1` is a Type Error.
                # Convert explicitly, then mutate with arithmetic.
                LET $raw: STRING = ECHO 100
                IMPORT [STD]
                LET $n: INT = INT($raw)
                $n = $n + 1
                # The declared type also converts plain strings on assignment.
                $n = "42"
                # Same crossing for decimals via FLOAT().
                LET $frac_str: STRING = ECHO 2.5
                LET $f: FLOAT = FLOAT($frac_str) + 0.25
                ASSERT_EQ $n 42
                ASSERT_EQ $f 2.75
            "#},
                },
            ],
        },
        CommandMeta {
            name: "ASYNC",
            syntax: "ASYNC <command...> | ASYNC { <commands> } | LET $var: HANDLE = ASYNC { <commands> }",
            summary: "Run steps in a background thread.",
            description: indoc! {r#"
                Runs a command or block of commands in a background thread with
                subshell isolation.

                Mutations (ENV, WORKDIR) stay within the block. With `LET`, stores a
                task handle for `AWAIT`.
            "#},
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
                    LET $task: HANDLE = ASYNC {
                        ECHO "built"
                    }
                    AWAIT $task
                "#},
                },
            ],
        },
        CommandMeta {
            name: "AWAIT",
            syntax: "AWAIT $var | LET $out: STRING = AWAIT $var",
            summary: "Join a background task.",
            description: indoc! {r#"
                Blocks until the named task completes. Propagates errors if the task failed.

                Bare `AWAIT $var` forwards the task's stdout to the parent stdout;
                `LET $out: STRING = AWAIT $var` captures it into `$out` instead (same
                UTF-8 and spilling rules as `LET $var: STRING = <command>`).
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "await",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $task: HANDLE = ASYNC ECHO "done"
                AWAIT $task
            "#},
                },
                Example {
                    name: "await capture",
                    fence_meta: None,
                    code: indoc! {r#"
                LET $task: HANDLE = ASYNC ECHO "done"
                LET $out: STRING = AWAIT $task
                ASSERT_EQ $out "done\n"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "CANCEL",
            syntax: "CANCEL $var",
            summary: "Synchronously cancel a background task.",
            description: indoc! {r#"
                Kills the named background task spawned via LET $var: HANDLE = ASYNC ....

                Blocking: returns only after the task thread has been joined and its OS
                process reaped, so no residual filesystem or stream mutation follows. A
                later AWAIT $var reports cancellation. Only named tasks can be cancelled.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "cancel",
                fence_meta: None,
                code: indoc! {r#"
                LET $task: HANDLE = ASYNC SLEEP 30s
                CANCEL $task
            "#},
            }],
        },
        CommandMeta {
            name: "TIMEOUT",
            syntax: "TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> } | TIMEOUT <duration> AWAIT $var",
            summary: "Enforce an execution deadline.",
            description: indoc! {r#"
                Aborts the wrapped step or block with a deadline error if it exceeds the
                duration (e.g. 500ms, 10s, 2m; a bare number means seconds).

                A blocking foreground process is killed.
            "#},
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
                    LET $budget: DURATION = "30s"
                    TIMEOUT $budget WRITE heartbeat.txt alive
                    LET $beat: STRING = READ heartbeat.txt
                    ASSERT_EQ $beat "alive"
                "#},
                },
            ],
        },
        CommandMeta {
            name: "FUNC",
            syntax: "FUNC NAME([$param: TYPE, ...]) { <commands> }",
            summary: "Define a user function.",
            description: indoc! {r#"
                Defines a user function with UPPERCASE name and explicitly typed
                parameters.

                Params bind by position, converting each argument to its
                declared parameter type before the body runs.
                Bodies run in a fresh variable scope; LETs inside do not leak. A nested
                FUNC definition is scoped to its block and reverts on exit. Names share
                one namespace with native and host-registered functions, which a FUNC
                may never shadow.

                Functions resolve like variables: a name is visible from its
                definition line, so recursion works but mutual recursion does
                not (the second name does not exist while the first body
                lowers). Calls name their module (`STD::GLOB(...)`) unless
                imported; see IMPORT.

                Invoke any function with one syntax: `NAME(...)` as a statement
                (discarding the value) or `LET $var: TYPE = NAME(...)` to capture
                the RETURN value (fallthrough without RETURN captures as "").
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[
                Example {
                    name: "func def call",
                    fence_meta: None,
                    code: indoc! {r#"
                FUNC GREET($name: STRING) {
                  RETURN $name
                }
                LET $res: STRING = GREET("ada")
                ASSERT_EQ $res "ada"
                # Statement form: parens stay, the value drops.
                GREET("bex")
            "#},
                },
                Example {
                    name: "call with pipes",
                    fence_meta: None,
                    code: indoc! {r#"
                # A pipe handle travels into a function as a typed argument
                # and is usable as a binding target in both directions.
                # `pipe:ch` constructs the handle; `$p` passes it on.
                FUNC DRAIN($q: PIPE) {
                  WITH_IO [stdin=$q] READ_LINE $line
                  RETURN $line
                }
                LET $p: PIPE = pipe:ch
                WITH_IO [stdout=$p] ECHO "payload"
                LET $got: STRING = DRAIN($p)
                ASSERT_EQ $got "payload"
            "#},
                },
            ],
        },
        CommandMeta {
            name: "RETURN",
            syntax: "RETURN [<expr>]",
            summary: "Return a value from a function.",
            description: indoc! {r#"
                Ends the nearest enclosing function call with a value.
                Bare `RETURN` with no expression yields `""`.

                Falling off the end without RETURN yields "". RETURN outside a function
                (including at top level or across an ASYNC boundary) is an error.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "return",
                fence_meta: None,
                code: indoc! {r#"
                FUNC PICK($flag: BOOL) {
                  IF $flag {
                    RETURN "yes"
                  }
                  RETURN "no"
                }
                LET $res: STRING = PICK(true)
                ASSERT_EQ $res "yes"
            "#},
            }],
        },
        CommandMeta {
            name: "WHILE",
            syntax: "WHILE <bool-expr> { <commands> }",
            summary: "Loop while a condition holds.",
            description: indoc! {r#"
                Re-evaluates a Bool condition each iteration (same is_truthy rule as IF;
                non-Bool is a type error).

                Each iteration runs in a fresh scope; mutate outer state with $var = ...
                so the next check observes it. BREAK exits the loop; CONTINUE skips to
                the next check.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "while loop",
                fence_meta: None,
                code: indoc! {r#"
                LET $done: BOOL = false
                WHILE !$done {
                  WRITE tick.txt "once"
                  $done = true
                }
                LET $tick: STRING = READ tick.txt
                ASSERT_EQ $tick "once"
            "#},
            }],
        },
        CommandMeta {
            name: "BREAK",
            syntax: "BREAK",
            summary: "Exit the innermost loop.",
            description: indoc! {r#"
                Exits the innermost enclosing FOR or WHILE loop.

                BREAK outside a loop, or across a FUNC or ASYNC boundary, is an error.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "break",
                fence_meta: None,
                code: indoc! {r#"
                FOR $x: STRING IN ["a", "b"] {
                  BREAK
                }
            "#},
            }],
        },
        CommandMeta {
            name: "CONTINUE",
            syntax: "CONTINUE",
            summary: "Skip to the next loop iteration.",
            description: indoc! {r#"
                Skips the rest of the innermost enclosing FOR or WHILE body and starts
                the next iteration.

                CONTINUE outside a loop, or across a FUNC or ASYNC boundary, is an error.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "continue",
                fence_meta: None,
                code: indoc! {r#"
                FOR $x: STRING IN ["a", "b"] {
                  CONTINUE
                }
            "#},
            }],
        },
        CommandMeta {
            name: KEYWORD_IMPORT,
            syntax: "IMPORT [<module>, ...] | IMPORT <module>",
            summary: "Bring module functions into bare-call scope.",
            description: indoc! {r#"
                Every function call names its module (`STD::GLOB(...)`,
                `MOCK::READ_CSV(...)`) unless the module is imported:
                `IMPORT [STD]` lets the rest of the scope call `GLOB(...)`
                bare. Calls resolve at parse time against `SCRIPT`
                definitions first, then imported modules; unknown modules,
                unknown functions, and unimported bare calls are parse
                errors, never runtime surprises.

                IMPORT is a lowering directive, not a step: it applies from
                its line to the enclosing block exit, then reverts, exactly
                like `LET` scoping but with no runtime footprint. Guards do
                not apply to it. Two imported modules exporting one name is
                an ambiguity error: qualify the call instead.

                `EXPORT` is reserved for future script-module support and
                cannot be used yet.
            "#},
            args: &[],
            flags: &[],
            default_output: None,
            examples: &[Example {
                name: "import",
                fence_meta: None,
                code: indoc! {r#"
                WRITE a.txt "hi \{{ env:WHO }}!"
                IMPORT [STD]
                FOR $x: STRING IN GLOB("*.txt") { EXPAND $x WHO=World }
                ASSERT_CONTAINS stdout "hi World!"
            "#},
            }],
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
            StepKind::AssertEq {
                hash,
                actual,
                expected,
            } => {
                if let Some(d) = hash {
                    write!(f, "ASSERT_EQ --hash {d} {}", fmt_assert_target(actual))?;
                } else {
                    write!(
                        f,
                        "ASSERT_EQ {} {}",
                        fmt_assert_target(actual),
                        fmt_value(
                            expected
                                .as_ref()
                                .expect("Display of ASSERT_EQ without --hash needs expected"),
                            quote_msg
                        )
                    )?;
                }
                Ok(())
            }
            StepKind::AssertContains { haystack, needle } => write!(
                f,
                "ASSERT_CONTAINS {} {}",
                fmt_assert_target(haystack),
                fmt_value(needle, quote_msg)
            ),
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
                key_type,
                var,
                var_type,
                in_expr,
                body,
            } => {
                match key_var {
                    Some(k) => {
                        let kt = key_type.as_deref().unwrap_or("STRING");
                        write!(
                            f,
                            "FOR ${}: {}, ${}: {} IN {} {{",
                            k, kt, var, var_type, in_expr
                        )?
                    }
                    None => write!(f, "FOR ${}: {} IN {} {{", var, var_type, in_expr)?,
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
            StepKind::Assign {
                var,
                decl_type,
                expr,
            } => {
                write!(f, "LET ${}: {} = {}", var, decl_type, expr)
            }
            StepKind::Set { var, expr } => write!(f, "${} = {}", var, expr),
            StepKind::AssignCapture {
                var,
                decl_type,
                cmd,
            } => {
                write!(f, "LET ${}: {} = {}", var, decl_type, cmd)
            }
            StepKind::AsyncBlock { body } => {
                write!(f, "ASYNC {{")?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::AssignAsync {
                var,
                decl_type,
                body,
            } => {
                write!(f, "LET ${}: {} = ASYNC {{", var, decl_type)?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::Await { var } => write!(f, "AWAIT ${}", var),
            StepKind::AwaitCapture {
                out_var,
                out_type,
                task_var,
            } => {
                write!(f, "LET ${}: {} = AWAIT ${}", out_var, out_type, task_var)
            }
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
            StepKind::FuncDef { name, params, body } => {
                let ps: Vec<String> = params
                    .iter()
                    .map(|(p, t)| format!("${}: {}", p, t))
                    .collect();
                write!(f, "FUNC {}({}) {{", name, ps.join(", "))?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::Call { name, args } => {
                let ps: Vec<String> = args.iter().map(|a| format!("{}", a)).collect();
                write!(f, "{}({})", name, ps.join(", "))
            }
            StepKind::Return { expr } => write!(f, "RETURN {}", expr),
            StepKind::While { cond, body } => {
                write!(f, "WHILE {} {{", cond)?;
                for s in body {
                    write!(f, "\n    {}", s)?;
                }
                write!(f, "\n}}")
            }
            StepKind::Break => write!(f, "BREAK"),
            StepKind::Continue => write!(f, "CONTINUE"),
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
    fn bare_let_without_type_points_at_typed_syntax() {
        let err = parse_err("LET $x = 1\n");
        assert!(err.contains("invalid syntax for command LET"), "{err}");
        assert!(err.contains("LET $name: STRING = <expr>"), "{err}");
    }

    #[test]
    fn space_before_paren_is_not_a_call() {
        // The call head and `(` must be contiguous: `ECHO (1 + 2)` is an
        // instruction, never a function invocation.
        let steps = parse_script("ECHO (1 + 2)\n", lower_command).expect("parses");
        assert_eq!(steps.len(), 1);
        assert!(
            !matches!(steps[0].kind, crate::ast::StepKind::Call { .. }),
            "space before paren must not route to a call: {:?}",
            steps[0].kind
        );
    }

    #[test]
    fn unknown_type_tag_parses_as_custom() {
        // Open type tags: declarations carry plain names and resolve
        // against the descriptor table at runtime, so host types need no
        // grammar change (see the custom_types core tests).
        let steps = parse_script("LET $x: FOO = 1\n", lower_command).expect("custom tag parses");
        let StepKind::Assign { decl_type, .. } = &steps[0].kind else {
            panic!("expected Assign, got {:?}", steps[0].kind);
        };
        assert_eq!(decl_type, "FOO");
    }

    #[test]
    fn bare_for_without_types_is_rejected() {
        let err = parse_err("FOR $i IN [1] { ECHO hi }\n");
        assert!(err.contains("FOR requires explicit types"), "{err}");
    }

    #[test]
    fn mutate_statement_parses_without_keyword() {
        let steps = parse_script("$y = 2\n", lower_command).expect("mutation parses");
        assert!(matches!(steps[0].kind, StepKind::Set { .. }));
    }

    #[test]
    fn set_keyword_is_rejected_with_mutation_hint() {
        let err = parse_err("SET $y = 2\n");
        assert!(err.contains("not a keyword"), "{err}");
        assert!(err.contains("$var = <expr>"), "{err}");
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
    fn func_def_requires_typed_uppercase_name() {
        let steps = parse_script(
            "FUNC GREET($name: STRING) {\n  RETURN $name\n}\n",
            lower_command,
        )
        .expect("func def parses");
        let StepKind::FuncDef { name, params, body } = &steps[0].kind else {
            panic!("expected FuncDef, got {:?}", steps[0].kind);
        };
        assert_eq!(name, "GREET");
        assert_eq!(
            params,
            &vec![("name".to_string(), "STRING".to_string())],
            "{params:?}"
        );
        assert!(matches!(body[0].kind, StepKind::Return { .. }));
    }

    #[test]
    fn lowercase_func_name_is_rejected() {
        let err = parse_err("FUNC greet($x: STRING) {\n  RETURN $x\n}\n");
        assert!(err.contains("FUNC"), "{err}");
    }

    #[test]
    fn call_and_while_lower_correctly() {
        let steps = parse_script(
            "FUNC GREET($name: STRING) {\n  RETURN $name\n}\nGREET(\"ada\")\n",
            lower_command,
        )
        .expect("call parses");
        assert!(
            matches!(&steps[1].kind, StepKind::Call { name, .. } if name == "SCRIPT::GREET"),
            "{:?}",
            steps[1].kind
        );
        let steps = parse_script(
            "FUNC GREET($a: STRING, $b: STRING) {\n  RETURN $a\n}\nGREET(\"ada\", \"bex\")\n",
            lower_command,
        )
        .expect("spaced call parses");
        assert!(
            matches!(&steps[1].kind, StepKind::Call { name, args } if name == "SCRIPT::GREET" && args.len() == 2),
            "{:?}",
            steps[1].kind
        );
        let steps =
            parse_script("WHILE !$done {\n  BREAK\n}\n", lower_command).expect("while parses");
        let StepKind::While { body, .. } = &steps[0].kind else {
            panic!("expected While, got {:?}", steps[0].kind);
        };
        assert!(matches!(body[0].kind, StepKind::Break));
    }

    #[test]
    fn let_capture_call_and_async_call_lower() {
        // A bare `NAME(...)` on the LET RHS stays an expression assignment;
        // only ASYNC/TIMEOUT/command captures produce AssignCapture.
        let steps = parse_script(
            "FUNC GREET($name: STRING) {\n  RETURN $name\n}\nLET $r: STRING = GREET(\"ada\")\n",
            lower_command,
        )
        .expect("capture call parses");
        let StepKind::Assign { var, expr, .. } = &steps[1].kind else {
            panic!("expected Assign, got {:?}", steps[1].kind);
        };
        assert_eq!(var, "r");
        assert!(
            matches!(expr, Expr::Call { name, .. } if name == "SCRIPT::GREET"),
            "{expr:?}"
        );
        let steps = parse_script(
            "FUNC GREET($name: STRING) {\n  RETURN $name\n}\nLET $t: HANDLE = ASYNC GREET(\"a\")\n",
            lower_command,
        )
        .expect("async call parses");
        assert!(
            matches!(&steps[1].kind, StepKind::AssignAsync { .. }),
            "{:?}",
            steps[1].kind
        );
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
                StepKind::Set { .. } => Some("MUTATION"),
                StepKind::AssignCapture { .. } => Some("LET"),
                StepKind::AwaitCapture { .. } => Some("AWAIT"),
                StepKind::AsyncBlock { .. } | StepKind::AssignAsync { .. } => Some("ASYNC"),
                StepKind::Await { .. } => Some("AWAIT"),
                StepKind::Cancel { .. } => Some("CANCEL"),
                StepKind::Timeout { .. } => Some("TIMEOUT"),
                StepKind::FuncDef { .. } => Some("FUNC"),
                // Bare `NAME(...)` calls share the `FUNC` reference page;
                // there is no call keyword to document on its own.
                StepKind::Call { .. } => None,
                StepKind::Return { .. } => Some("RETURN"),
                StepKind::While { .. } => Some("WHILE"),
                StepKind::Break => Some("BREAK"),
                StepKind::Continue => Some("CONTINUE"),
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
                | StepKind::AssertEq { .. }
                | StepKind::AssertContains { .. }
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
                key_type: None,
                var: "i".to_string(),
                var_type: "STRING".to_string(),
                in_expr: Expr::Literal(Value::bool(true)),
                body: Vec::new(),
            },
            StepKind::If {
                cond: Box::new(Expr::Literal(Value::bool(true))),
                then_body: Vec::new(),
                else_ifs: Vec::new(),
                else_body: None,
            },
            StepKind::Assign {
                var: "v".to_string(),
                decl_type: "BOOL".to_string(),
                expr: Expr::Literal(Value::bool(true)),
            },
            StepKind::Set {
                var: "v".to_string(),
                expr: Expr::Literal(Value::bool(true)),
            },
            StepKind::AssignCapture {
                var: "v".to_string(),
                decl_type: "STRING".to_string(),
                cmd: Box::new(StepKind::Echo(crate::ast::Arg::String(
                    "x".to_string(),
                    false,
                ))),
            },
            StepKind::AwaitCapture {
                out_var: "o".to_string(),
                out_type: "STRING".to_string(),
                task_var: "t".to_string(),
            },
            StepKind::AsyncBlock { body: Vec::new() },
            StepKind::AssignAsync {
                var: "t".to_string(),
                decl_type: "HANDLE".to_string(),
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
            StepKind::FuncDef {
                name: "F".to_string(),
                params: Vec::new(),
                body: Vec::new(),
            },
            StepKind::Call {
                name: "F".to_string(),
                args: Vec::new(),
            },
            StepKind::Return {
                expr: Box::new(Expr::Literal(Value::bool(true))),
            },
            StepKind::While {
                cond: Box::new(Expr::Literal(Value::bool(true))),
                body: Vec::new(),
            },
            StepKind::Break,
            StepKind::Continue,
        ];
        let registry = all_structural_metadata();
        for kind in &dummies {
            // Bare calls share the FUNC reference page and map to None.
            let Some(name) = metadata_name(kind) else {
                continue;
            };
            assert!(
                registry.iter().any(|meta| meta.name == name),
                "no structural metadata entry for {}",
                name
            );
        }
    }
}
