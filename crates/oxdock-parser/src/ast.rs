use std::collections::HashMap;
use std::sync::Arc;

pub use crate::commands::StepKind;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    InheritEnv,
    Workdir,
    Workspace,
    Env,
    Echo,
    Run,
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
    Platform { target: PlatformGuard },
    EnvExists { key: String },
    EnvEquals { key: String, value: String },
    StaticBool { value: String },
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
    /// Expandable string. The `bool` indicates whether the argument was
    /// quoted in the source (`true`) or unquoted (`false`). Quoted arguments
    /// that start with `--` are positional, not flags.
    String(String, bool),
    /// Expression — resolved at runtime via evaluate_expr.
    Expr(Expr),
}

impl Arg {
    pub fn as_str(&self) -> &str {
        match self {
            Arg::String(s, _) => s,
            Arg::Expr(_) => "",
        }
    }

    pub fn is_quoted(&self) -> bool {
        matches!(self, Arg::String(_, true))
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::String(s, false)
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::String(s.to_string(), false)
    }
}

impl std::fmt::Display for Arg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arg::String(s, _) => write!(f, "{}", s),
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
    /// Handle to a background ASYNC task. The `u64` is the task ID
    /// used to look up the handle in `ExecState.named_tasks`.
    TaskHandle(u64),
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
        Guard::EnvExists { key } => env.get_env(key).map(|v| !v.is_empty()).unwrap_or(false),
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

pub fn guard_option_allows(expr: Option<&GuardExpr>, env: &impl EnvLookup) -> bool {
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
            Value::TaskHandle(id) => write!(f, "task#{}", id),
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
