use std::collections::HashMap;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    InheritEnv,
    Workdir,
    Workspace,
    Env,
    Echo,
    Run,
    RunBg,
    Copy,
    WithIo,
    CopyGit,
    HashSha256,
    Symlink,
    Mkdir,
    Ls,
    Cwd,
    Read,
    Write,
    Append,
    Expand,
    AssertFile,
    AssertDir,
    AssertAbsent,
    AssertStdout,
    Exit,
}

pub const COMMANDS: &[Command] = &[
    Command::InheritEnv,
    Command::Workdir,
    Command::Workspace,
    Command::Env,
    Command::Echo,
    Command::Run,
    Command::RunBg,
    Command::Copy,
    Command::WithIo,
    Command::CopyGit,
    Command::HashSha256,
    Command::Symlink,
    Command::Mkdir,
    Command::Ls,
    Command::Cwd,
    Command::Read,
    Command::Write,
    Command::Append,
    Command::Expand,
    Command::AssertFile,
    Command::AssertDir,
    Command::AssertAbsent,
    Command::AssertStdout,
    Command::Exit,
];

impl Command {
    pub const fn as_str(self) -> &'static str {
        match self {
            Command::InheritEnv => "INHERIT_ENV",
            Command::Workdir => "WORKDIR",
            Command::Workspace => "WORKSPACE",
            Command::Env => "ENV",
            Command::Echo => "ECHO",
            Command::Run => "RUN",
            Command::RunBg => "RUN_BG",
            Command::Copy => "COPY",
            Command::WithIo => "WITH_IO",
            Command::CopyGit => "COPY_GIT",
            Command::HashSha256 => "HASH_SHA256",
            Command::Symlink => "SYMLINK",
            Command::Mkdir => "MKDIR",
            Command::Ls => "LS",
            Command::Cwd => "CWD",
            Command::Read => "READ",
            Command::Write => "WRITE",
            Command::Append => "APPEND",
            Command::Expand => "EXPAND",
            Command::AssertFile => "ASSERT_FILE",
            Command::AssertDir => "ASSERT_DIR",
            Command::AssertAbsent => "ASSERT_ABSENT",
            Command::AssertStdout => "ASSERT_STDOUT",
            Command::Exit => "EXIT",
        }
    }

    pub const fn syntax(self) -> &'static str {
        match self {
            Command::InheritEnv => "INHERIT_ENV [KEY1, KEY2, ...]",
            Command::Workdir => "WORKDIR <path>",
            Command::Workspace => "WORKSPACE SNAPSHOT|LOCAL",
            Command::Env => "ENV KEY=value",
            Command::Echo => "ECHO <message>",
            Command::Run => "RUN <command...>",
            Command::RunBg => "RUN_BG <command...>",
            Command::Copy => "COPY [--from-current-workspace] <from> <to>",
            Command::CopyGit => "COPY_GIT [--include-dirty] <rev> <src> <dst>",
            Command::WithIo => "WITH_IO [bindings] [command | { block }]",
            Command::HashSha256 => "HASH_SHA256 <path>",
            Command::Symlink => "SYMLINK <from> <to>",
            Command::Mkdir => "MKDIR <path>",
            Command::Ls => "LS [<path>]",
            Command::Cwd => "CWD",
            Command::Read => "READ [<path>]",
            Command::Write => "WRITE <path> [<contents>]",
            Command::Append => "APPEND <path> [<contents>]",
            Command::Expand => "EXPAND [<path>] [<KEY=val> ...]",
            Command::AssertFile => "ASSERT_FILE [--hash <sha256>] <path> [<expected>]",
            Command::AssertDir => "ASSERT_DIR <path>",
            Command::AssertAbsent => "ASSERT_ABSENT <path>",
            Command::AssertStdout => "ASSERT_STDOUT <substring>",
            Command::Exit => "EXIT <code>",
        }
    }

    pub const fn expects_inner_command(self) -> bool {
        matches!(self, Command::WithIo)
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "INHERIT_ENV" => Some(Command::InheritEnv),
            "WORKDIR" => Some(Command::Workdir),
            "WORKSPACE" => Some(Command::Workspace),
            "ENV" => Some(Command::Env),
            "ECHO" => Some(Command::Echo),
            "RUN" => Some(Command::Run),
            "RUN_BG" => Some(Command::RunBg),
            "COPY" => Some(Command::Copy),
            "WITH_IO" => Some(Command::WithIo),
            "COPY_GIT" => Some(Command::CopyGit),
            "HASH_SHA256" => Some(Command::HashSha256),
            "SYMLINK" => Some(Command::Symlink),
            "MKDIR" => Some(Command::Mkdir),
            "LS" => Some(Command::Ls),
            "CWD" => Some(Command::Cwd),
            "READ" => Some(Command::Read),
            "WRITE" => Some(Command::Write),
            "APPEND" => Some(Command::Append),
            "EXPAND" => Some(Command::Expand),
            "ASSERT_FILE" => Some(Command::AssertFile),
            "ASSERT_DIR" => Some(Command::AssertDir),
            "ASSERT_ABSENT" => Some(Command::AssertAbsent),
            "ASSERT_STDOUT" => Some(Command::AssertStdout),
            "EXIT" => Some(Command::Exit),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PlatformGuard {
    Unix,
    Windows,
    Macos,
    Linux,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Guard {
    Platform {
        target: PlatformGuard,
    },
    EnvExists {
        key: String,
    },
    EnvEquals {
        key: String,
        value: String,
    },
    StaticBool {
        value: String,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum GuardExpr {
    Predicate(Guard),
    All(Vec<GuardExpr>),
    Or(Vec<GuardExpr>),
    Not(Box<GuardExpr>),
}

impl GuardExpr {
    pub fn all(exprs: Vec<GuardExpr>) -> GuardExpr {
        let mut flat = Vec::new();
        for expr in exprs {
            match expr {
                GuardExpr::All(children) => flat.extend(children),
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => panic!("GuardExpr::all requires at least one expression"),
            1 => flat.into_iter().next().unwrap(),
            _ => GuardExpr::All(flat),
        }
    }

    pub fn or(exprs: Vec<GuardExpr>) -> GuardExpr {
        let mut flat = Vec::new();
        for expr in exprs {
            match expr {
                GuardExpr::Or(children) => flat.extend(children),
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => panic!("GuardExpr::or requires at least one expression"),
            1 => flat.into_iter().next().unwrap(),
            _ => GuardExpr::Or(flat),
        }
    }

    pub fn invert(expr: GuardExpr) -> GuardExpr {
        match expr {
            GuardExpr::Not(inner) => *inner,
            other => GuardExpr::Not(Box::new(other)),
        }
    }
}

impl std::ops::Not for GuardExpr {
    type Output = GuardExpr;

    fn not(self) -> GuardExpr {
        match self {
            GuardExpr::Not(inner) => *inner,
            other => GuardExpr::Not(Box::new(other)),
        }
    }
}

impl From<Guard> for GuardExpr {
    fn from(guard: Guard) -> Self {
        GuardExpr::Predicate(guard)
    }
}

/// A command argument — either an expandable string or an expression.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Arg {
    /// Expandable string — `{{ $var }}` and `{{ env:KEY }}` expanded at runtime by expand_string.
    /// Backslash escapes (`\n`, `\\`, etc.) are also processed.
    String(String),
    /// Expression — resolved at runtime via evaluate_expr.
    Expr(Expr),
}

impl Arg {
    pub fn as_str(&self) -> &str {
        match self {
            Arg::String(s) => s,
            Arg::Expr(_) => "",
        }
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::String(s)
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::String(s.to_string())
    }
}

impl std::fmt::Display for Arg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arg::String(s) => write!(f, "{}", s),
            Arg::Expr(e) => write!(f, "{}", e),
        }
    }
}

impl AsRef<str> for Arg {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for Arg {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Arg {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum IoStream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IoBinding {
    pub stream: IoStream,
    pub pipe: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    List(Vec<Value>),
    Map(std::collections::BTreeMap<String, Value>),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CompareOp {
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LogicalOp {
    And,
    Or,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Expr {
    Literal(Value),
    Var(String),
    KeyPath {
        base: String,
        keys: Vec<String>,
    },
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Compare {
        op: CompareOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StepKind {
    Workdir(Arg),
    Workspace(WorkspaceTarget),
    Env {
        key: String,
        value: Arg,
    },
    /// Directive to inherit a selective list of environment variables from the host.
    /// This is intended to be declared in the prelude/top-level only.
    InheritEnv {
        keys: Vec<String>,
    },
    Run(Arg),
    Echo(Arg),
    RunBg(Arg),
    Copy {
        from_current_workspace: bool,
        from: Arg,
        to: Arg,
    },
    Symlink {
        from: Arg,
        to: Arg,
    },
    Mkdir(Arg),
    Ls(Option<Arg>),
    Cwd,
    Read(Option<Arg>),
    Write {
        path: Arg,
        contents: Option<Arg>,
    },
    Append {
        path: Arg,
        contents: Option<Arg>,
    },
    /// EXPAND `[path]` `[KEY=val ...]`
    ///
    /// Reads file or stdin, expands `{{ env:KEY }}` placeholders, outputs to stdout.
    /// Explicit KEY=val arguments override env vars.
    Expand {
        path: Option<Arg>,
        overrides: Vec<(String, Arg)>,
    },
    /// Verify a workspace file exists and, when `hash` or `contents` is
    /// present, matches it. The grammar guarantees the invariant: `hash` set
    /// implies a 64-hex digest and no `contents`.
    AssertFile {
        hash: Option<String>,
        path: Arg,
        contents: Option<Arg>,
    },
    AssertDir(Arg),
    AssertAbsent(Arg),
    AssertStdout(Arg),
    WithIo {
        bindings: Vec<IoBinding>,
        cmd: Box<StepKind>,
    },
    WithIoBlock {
        bindings: Vec<IoBinding>,
    },
    CopyGit {
        rev: Arg,
        from: Arg,
        to: Arg,
        include_dirty: bool,
    },
    HashSha256 {
        path: Arg,
    },
    Exit(i32),
    For {
        key_var: Option<String>,
        var: String,
        in_expr: Expr,
        body: Vec<Step>,
    },
    If {
        cond: Box<Expr>,
        then_body: Vec<Step>,
        else_ifs: Vec<(Box<Expr>, Vec<Step>)>,
        else_body: Option<Vec<Step>>,
    },
    Assign {
        var: String,
        expr: Expr,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Step {
    pub guard: Option<GuardExpr>,
    pub kind: StepKind,
    pub scope_enter: usize,
    pub scope_exit: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Snapshot,
    Local,
}

fn platform_matches(target: PlatformGuard) -> bool {
    #[allow(clippy::disallowed_macros)]
    match target {
        PlatformGuard::Unix => cfg!(unix),
        PlatformGuard::Windows => cfg!(windows),
        PlatformGuard::Macos => cfg!(target_os = "macos"),
        PlatformGuard::Linux => cfg!(target_os = "linux"),
    }
}

pub trait EnvLookup {
    fn get_env(&self, key: &str) -> Option<&str>;
}

impl EnvLookup for HashMap<String, String> {
    fn get_env(&self, key: &str) -> Option<&str> {
        self.get(key).map(|s| s.as_str())
    }
}

impl EnvLookup for Arc<HashMap<String, String>> {
    fn get_env(&self, key: &str) -> Option<&str> {
        (**self).get_env(key)
    }
}

pub fn guard_allows(guard: &Guard, env: &impl EnvLookup) -> bool {
    match guard {
        Guard::Platform { target } => platform_matches(*target),
        Guard::EnvExists { key } => env
            .get_env(key)
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        Guard::EnvEquals { key, value } => env
            .get_env(key)
            .map(|v| v == value.as_str())
            .unwrap_or(false),
        Guard::StaticBool { value } => value.parse::<bool>().unwrap_or(false),
    }
}

pub fn guard_expr_allows(expr: &GuardExpr, env: &impl EnvLookup) -> bool {
    match expr {
        GuardExpr::Predicate(guard) => guard_allows(guard, env),
        GuardExpr::All(children) => children.iter().all(|g| guard_expr_allows(g, env)),
        GuardExpr::Or(children) => children.iter().any(|g| guard_expr_allows(g, env)),
        GuardExpr::Not(child) => !guard_expr_allows(child, env),
    }
}

pub fn guard_option_allows(
    expr: Option<&GuardExpr>,
    env: &impl EnvLookup,
) -> bool {
    match expr {
        Some(e) => guard_expr_allows(e, env),
        None => true,
    }
}

use std::fmt;

impl fmt::Display for PlatformGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformGuard::Unix => write!(f, "unix"),
            PlatformGuard::Windows => write!(f, "windows"),
            PlatformGuard::Macos => write!(f, "macos"),
            PlatformGuard::Linux => write!(f, "linux"),
        }
    }
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Guard::Platform { target } => write!(f, "{}", target),
            Guard::EnvExists { key } => write!(f, "env:{}", key),
            Guard::EnvEquals { key, value } => write!(f, "eq(env:{}, {})", key, value),
            Guard::StaticBool { value } => write!(f, "bool:{}", value),
        }
    }
}

impl fmt::Display for WorkspaceTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceTarget::Snapshot => write!(f, "SNAPSHOT"),
            WorkspaceTarget::Local => write!(f, "LOCAL"),
        }
    }
}

fn quote_arg(s: &str) -> String {
    // Strict quoting avoids parser ambiguity when commands accept additional payloads
    // (e.g. WRITE path <payload>) so arguments are never mistaken for subsequent tokens.
    // Also quote if it starts with a digit to avoid invalid Rust tokens (e.g. 0o8) in macros.
    let is_safe = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
        // Avoid unquoted args that equal command keywords (they would be parsed as commands
        // when reconstructed from TokenStream). Quote them to preserve intent.
        && super::Command::parse(s).is_none();
    if is_safe && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn quote_msg(s: &str) -> String {
    // Strict quoting to ensure round-trip stability through TokenStream (macro input).
    // The macro input reconstructor removes spaces around "sticky" characters (/-.:=)
    // and collapses multiple spaces, so we must quote strings containing them.
    // We also quote strings with spaces to be safe, as TokenStream does not preserve whitespace.
    // Also quote if it starts with a digit to avoid invalid Rust tokens.
    let is_safe = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
        // As with args, avoid leaving bare tokens that match command names.
        && super::Command::parse(s).is_none();

    if is_safe && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn quote_run(s: &str) -> String {
    // For RUN commands, we want to preserve the raw string as much as possible.
    // However, to ensure round-trip stability through TokenStream (macro input),
    // we must ensure that the generated string is a valid sequence of Rust tokens.
    // Invalid tokens (like 0o8) must be quoted.
    // Also, sticky characters (like -) can merge with previous tokens in macro input,
    // so we quote words starting with them to ensure separation.

    let force_full_quote = s.is_empty()
        || s.chars().any(|c| c == ';' || c == '\n' || c == '\r')
        || s.contains("//")
        || s.contains("/*");

    if force_full_quote {
        return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    }

    s.split(' ')
        .map(|word| {
            let needs_quote = word.starts_with(|c: char| c.is_ascii_digit())
                || word.starts_with(['/', '.', '-', ':', '=']);
            if needs_quote {
                format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_io_binding(binding: &IoBinding) -> String {
    let stream = match binding.stream {
        IoStream::Stdin => "stdin",
        IoStream::Stdout => "stdout",
        IoStream::Stderr => "stderr",
    };
    if let Some(pipe) = &binding.pipe {
        format!("{}=pipe:{}", stream, pipe)
    } else {
        stream.to_string()
    }
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepKind::InheritEnv { keys } => {
                write!(f, "INHERIT_ENV [{}]", keys.join(", "))
            }
            StepKind::Workdir(arg) => write!(f, "WORKDIR {}", quote_arg(arg.as_str())),
            StepKind::Workspace(target) => write!(f, "WORKSPACE {}", target),
            StepKind::Env { key, value } => write!(f, "ENV {}={}", key, quote_arg(value.as_str())),
            StepKind::Run(cmd) => write!(f, "RUN {}", quote_run(cmd.as_str())),
            StepKind::Echo(msg) => write!(f, "ECHO {}", quote_msg(msg.as_str())),
            StepKind::RunBg(cmd) => write!(f, "RUN_BG {}", quote_run(cmd.as_str())),
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
            StepKind::Symlink { from, to } => {
                write!(
                    f,
                    "SYMLINK {} {}",
                    quote_arg(from.as_str()),
                    quote_arg(to.as_str())
                )
            }
            StepKind::Mkdir(arg) => write!(f, "MKDIR {}", quote_arg(arg.as_str())),
            StepKind::Ls(arg) => {
                write!(f, "LS")?;
                if let Some(a) = arg {
                    write!(f, " {}", quote_arg(a.as_str()))?;
                }
                Ok(())
            }
            StepKind::Cwd => write!(f, "CWD"),
            StepKind::Read(arg) => {
                write!(f, "READ")?;
                if let Some(a) = arg {
                    write!(f, " {}", quote_arg(a.as_str()))?;
                }
                Ok(())
            }
            StepKind::Write { path, contents } => {
                write!(f, "WRITE {}", quote_arg(path.as_str()))?;
                if let Some(body) = contents {
                    write!(f, " {}", quote_msg(body.as_str()))?;
                }
                Ok(())
            }
            StepKind::Append { path, contents } => {
                write!(f, "APPEND {}", quote_arg(path.as_str()))?;
                if let Some(body) = contents {
                    write!(f, " {}", quote_msg(body.as_str()))?;
                }
                Ok(())
            }
            StepKind::Expand { path, overrides } => {
                write!(f, "EXPAND")?;
                if let Some(p) = path {
                    write!(f, " {}", quote_arg(p.as_str()))?;
                }
                for (key, value) in overrides {
                    write!(f, " {}={}", key, quote_arg(value.as_str()))?;
                }
                Ok(())
            }
            StepKind::AssertFile {
                hash,
                path,
                contents,
            } => {
                if let Some(digest) = hash {
                    write!(
                        f,
                        "ASSERT_FILE --hash {} {}",
                        digest,
                        quote_arg(path.as_str())
                    )
                } else {
                    write!(f, "ASSERT_FILE {}", quote_arg(path.as_str()))?;
                    if let Some(body) = contents {
                        write!(f, " {}", quote_msg(body.as_str()))?;
                    }
                    Ok(())
                }
            }
            StepKind::AssertDir(arg) => write!(f, "ASSERT_DIR {}", quote_arg(arg.as_str())),
            StepKind::AssertAbsent(arg) => write!(f, "ASSERT_ABSENT {}", quote_arg(arg.as_str())),
            StepKind::AssertStdout(msg) => write!(f, "ASSERT_STDOUT {}", quote_msg(msg.as_str())),
            StepKind::WithIo { bindings, cmd } => {
                let parts: Vec<String> = bindings.iter().map(format_io_binding).collect();
                write!(f, "WITH_IO [{}] {}", parts.join(", "), cmd)
            }
            StepKind::WithIoBlock { bindings } => {
                let parts: Vec<String> = bindings.iter().map(format_io_binding).collect();
                write!(f, "WITH_IO [{}] {{...}}", parts.join(", "))
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
            StepKind::Exit(code) => write!(f, "EXIT {}", code),
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
                for step in body {
                    write!(f, "\n    {}", step)?;
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
                for step in then_body {
                    write!(f, "\n    {}", step)?;
                }
                write!(f, " }}")?;
                for (cond, body) in else_ifs {
                    write!(f, " ELSE IF {} {{", cond)?;
                    for step in body {
                        write!(f, "\n    {}", step)?;
                    }
                    write!(f, " }}")?;
                }
                if let Some(body) = else_body {
                    write!(f, " ELSE {{")?;
                    for step in body {
                        write!(f, "\n    {}", step)?;
                    }
                    write!(f, " }}")?;
                }
                Ok(())
            }
            StepKind::Assign { var, expr } => {
                write!(f, "LET ${} = {}", var, expr)
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::String(s) => write!(f, "\"{}\"", s),
            Value::Int(i) => write!(f, "{}", i),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Value::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, "}}")
            }
            Value::Bool(b) => write!(f, "{}", b),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(v) => write!(f, "{}", v),
            Expr::Var(name) => write!(f, "${}", name),
            Expr::KeyPath { base, keys } => {
                write!(f, "${}", base)?;
                for key in keys {
                    write!(f, ".{}", key)?;
                }
                Ok(())
            }
            Expr::Call { name, args } => {
                write!(f, "{}(", name)?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", arg)?;
                }
                write!(f, ")")
            }
            Expr::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Expr::Map(entries) => {
                write!(f, "{{")?;
                for (i, (key, val)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{}\": {}", key, val)?;
                }
                write!(f, "}}")
            }
            Expr::Compare { op, left, right } => {
                write!(f, "{} {} {}", left, op, right)
            }
            Expr::Logical { op, left, right } => {
                write!(f, "({} {} {})", left, op, right)
            }
        }
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompareOp::Eq => write!(f, "=="),
            CompareOp::Ne => write!(f, "!="),
        }
    }
}

impl fmt::Display for LogicalOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicalOp::And => write!(f, "&&"),
            LogicalOp::Or => write!(f, "||"),
        }
    }
}

enum GuardDisplayContext {
    Root,
    InAnyArg,
    InNot,
    InAll,
}

impl GuardExpr {
    fn fmt_with_ctx(&self, f: &mut fmt::Formatter<'_>, ctx: GuardDisplayContext) -> fmt::Result {
        match self {
            GuardExpr::Predicate(guard) => write!(f, "{}", guard),
            GuardExpr::All(children) => {
                let wrap = matches!(
                    ctx,
                    GuardDisplayContext::InAnyArg | GuardDisplayContext::InNot
                ) && children.len() > 1;
                if wrap {
                    write!(f, "(")?;
                }
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    child.fmt_with_ctx(f, GuardDisplayContext::InAll)?;
                }
                if wrap {
                    write!(f, ")")?;
                }
                Ok(())
            }
            GuardExpr::Or(children) => {
                write!(f, "any(")?;
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    child.fmt_with_ctx(f, GuardDisplayContext::InAnyArg)?;
                }
                write!(f, ")")
            }
            GuardExpr::Not(child) => {
                write!(f, "not(")?;
                child.fmt_with_ctx(f, GuardDisplayContext::InNot)?;
                write!(f, ")")
            }
        }
    }
}

impl fmt::Display for GuardExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_with_ctx(f, GuardDisplayContext::Root)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(expr) = &self.guard {
            write!(f, "[{}] ", expr)?;
        }
        write!(f, "{}", self.kind)
    }
}
