use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

pub use crate::commands::{AssertTarget, StepKind};
use crate::constants::{
    KEYWORD_ASYNC, KEYWORD_AWAIT, KEYWORD_BREAK, KEYWORD_CANCEL, KEYWORD_CONTINUE, KEYWORD_ELSE,
    KEYWORD_EXPORT, KEYWORD_FOR, KEYWORD_FUNC, KEYWORD_IF, KEYWORD_IMPORT, KEYWORD_LET,
    KEYWORD_RETURN, KEYWORD_WHILE,
};

/// One module's function surface for parse-time call resolution: the base
/// names it exports. RPN eligibility needs no table: calls compile
/// generically and the runtime registry gates evaluation per entry.
#[derive(Debug, Clone, Default)]
pub struct ModuleFuncs {
    /// Base function names exported by the module.
    pub functions: HashSet<String>,
}

/// Parse-time function provenance: module name to surface. A `None` entry
/// marks an opaque module (declared but membership unknown, e.g. the
/// `oxdock!` macro's `modules:` prefix): qualified calls pass through for
/// runtime checking, and a lone opaque import determines bare-call targets.
#[derive(Debug, Clone, Default)]
pub struct ModuleTable {
    pub modules: HashMap<String, Option<ModuleFuncs>>,
}

impl ModuleTable {
    /// Base names exported by every known (non-opaque) module. Backs the
    /// `FUNC` shadow check: a script definition colliding with any of these
    /// fails at parse time, exactly like the old flat reserved set.
    pub fn reserved_base_names(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for funcs in self.modules.values().flatten() {
            out.extend(funcs.functions.iter().cloned());
        }
        out
    }
}

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
    ReadLine,
    Write,
    Append,
    Expand,
    AssertEq,
    AssertContains,
    Exit,
    Async,
    Timeout,
    Sleep,
    Connect,
    Listen,
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
    Command::ReadLine,
    Command::Write,
    Command::Append,
    Command::Expand,
    Command::AssertEq,
    Command::AssertContains,
    Command::Exit,
    Command::Timeout,
    Command::Sleep,
    Command::Connect,
    Command::Listen,
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
            Command::ReadLine => "READ_LINE",
            Command::Write => "WRITE",
            Command::Append => "APPEND",
            Command::Expand => "EXPAND",
            Command::AssertEq => "ASSERT_EQ",
            Command::AssertContains => "ASSERT_CONTAINS",
            Command::Exit => "EXIT",
            Command::Async => "ASYNC",
            Command::Timeout => "TIMEOUT",
            Command::Sleep => "SLEEP",
            Command::Connect => "CONNECT",
            Command::Listen => "LISTEN",
        }
    }

    pub const fn syntax(self) -> &'static str {
        match self {
            Command::InheritEnv => "INHERIT_ENV [KEY1, KEY2, ...]",
            Command::Workdir => "WORKDIR <path>",
            Command::Workspace => "WORKSPACE SNAPSHOT|LOCAL",
            Command::Env => "ENV KEY=value",
            Command::Echo => "ECHO <message>",
            Command::Run => "RUN <command...> | RUN [\"exe\", \"arg\", ...]",
            Command::Copy => "COPY [--from-current-workspace] <from> <to>",
            Command::CopyGit => "COPY_GIT [--include-dirty] <rev> <src> <dst>",
            Command::WithIo => "WITH_IO [bindings] [command | { block }]",
            Command::HashSha256 => "HASH_SHA256 <path>",
            Command::Symlink => "SYMLINK <from> <to>",
            Command::Mkdir => "MKDIR <path>",
            Command::Ls => "LS [<path>]",
            Command::Cwd => "CWD",
            Command::Read => "READ [<path>]",
            Command::ReadLine => "READ_LINE $var",
            Command::Write => "WRITE <path> [<contents>]",
            Command::Append => "APPEND <path> [<contents>]",
            Command::Expand => "EXPAND [<path>] [<KEY=val> ...]",
            Command::AssertEq => "ASSERT_EQ [--hash <sha256>] <actual> <expected>",
            Command::AssertContains => "ASSERT_CONTAINS <haystack> <needle>",
            Command::Exit => "EXIT <code>",
            Command::Async => "ASYNC <command...> | ASYNC { <commands> }",
            Command::Timeout => {
                "TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> }"
            }
            Command::Sleep => "SLEEP <duration>",
            Command::Connect => "CONNECT <host:port> [--timeout <duration>]",
            Command::Listen => "LISTEN [host:]port",
        }
    }

    pub const fn expects_inner_command(self) -> bool {
        matches!(self, Command::WithIo | Command::Async | Command::Timeout)
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
            "READ_LINE" => Some(Command::ReadLine),
            "WRITE" => Some(Command::Write),
            "APPEND" => Some(Command::Append),
            "EXPAND" => Some(Command::Expand),
            "ASSERT_EQ" => Some(Command::AssertEq),
            "ASSERT_CONTAINS" => Some(Command::AssertContains),
            "EXIT" => Some(Command::Exit),
            "ASYNC" => Some(Command::Async),
            "TIMEOUT" => Some(Command::Timeout),
            "SLEEP" => Some(Command::Sleep),
            "CONNECT" => Some(Command::Connect),
            "LISTEN" => Some(Command::Listen),
            _ => None,
        }
    }

    /// Whether a bare word opens a new statement in either parse pathway.
    /// Covers plain commands plus [`STRUCTURAL_KEYWORDS`]. Single source
    /// of truth for `Display` quoting and token-stream line splitting, which
    /// must agree or round-trips break.
    pub(crate) fn is_statement_keyword(s: &str) -> bool {
        Command::parse(s).is_some() || STRUCTURAL_KEYWORDS.contains(&s)
    }
}

/// Statement starters parsed by PEG rules rather than command lowering
/// (`dsl.pest`, token-walker branches), living outside the [`Command`]
/// enum. Canonical registry backing `Command::is_statement_keyword`;
/// iterate this (plus [`crate::all_metadata`] names) instead of
/// hardcoding keyword lists elsewhere.
pub const STRUCTURAL_KEYWORDS: &[&str] = &[
    KEYWORD_LET,
    KEYWORD_FOR,
    KEYWORD_IF,
    KEYWORD_ELSE,
    KEYWORD_ASYNC,
    KEYWORD_AWAIT,
    KEYWORD_CANCEL,
    KEYWORD_FUNC,
    KEYWORD_RETURN,
    KEYWORD_WHILE,
    KEYWORD_BREAK,
    KEYWORD_CONTINUE,
    KEYWORD_IMPORT,
    KEYWORD_EXPORT,
];

/// Clause keywords that open no statement and need no `Display` quoting
/// (`IN` only heads `FOR` iterations). Declared alongside
/// [`STRUCTURAL_KEYWORDS`] so grammar-conformance tests never hardcode
/// exception lists of their own.
pub const CLAUSE_KEYWORDS: &[&str] = &["IN"];

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
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// Expandable string. The `bool` indicates whether the argument was
    /// quoted in the source (`true`) or unquoted (`false`). Quoted arguments
    /// that start with `--` are positional, not flags.
    String(String, bool),
    /// Expression — resolved at runtime via evaluate_expr.
    Expr(Expr),
    /// Mixed literal/expression value (e.g. `KEY={{ $x }} tail`). Fragments
    /// resolve independently at runtime and concatenate with no added
    /// separator — inter-fragment gaps are already materialized as `Text`.
    Parts(Vec<ArgPart>),
}

/// One fragment of a mixed [`Arg::Parts`] value.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgPart {
    /// Literal text. The `bool` marks source-quoted regions (exact bytes);
    /// unquoted text carries single-space-normalized gaps.
    Text(String, bool),
    /// Typed expression — resolved via evaluate_expr, never stringified.
    Expr(Expr),
}

impl Arg {
    pub fn as_str(&self) -> &str {
        match self {
            Arg::String(s, _) => s,
            // Expressions and mixed values have no single borrowed string;
            // use `render()` for an owned display form.
            Arg::Expr(_) | Arg::Parts(_) => "",
        }
    }

    /// Owned display form: `String` verbatim, `Expr` as source (`$x`),
    /// `Parts` as fragment concatenation. Used for diagnostics and Display;
    /// runtime resolution must match on variants instead (see resolve_arg).
    pub fn render(&self) -> String {
        match self {
            Arg::String(s, _) => s.clone(),
            Arg::Expr(e) => e.to_string(),
            Arg::Parts(parts) => parts.iter().map(ArgPart::render).collect(),
        }
    }

    pub fn is_quoted(&self) -> bool {
        matches!(self, Arg::String(_, true))
    }
}

impl ArgPart {
    pub fn render(&self) -> String {
        match self {
            ArgPart::Text(s, _) => s.clone(),
            ArgPart::Expr(e) => e.to_string(),
        }
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
            Arg::Parts(parts) => {
                for part in parts {
                    match part {
                        ArgPart::Text(s, _) => write!(f, "{}", s)?,
                        ArgPart::Expr(e) => write!(f, "{}", e)?,
                    }
                }
                Ok(())
            }
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IoStream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IoBinding {
    pub stream: IoStream,
    pub pipe: Option<PipeTarget>,
}

/// A pipe endpoint for a `WITH_IO` binding: either a literal `pipe:name`
/// or a `$var` holding a `PIPE` value, resolved against the live pipe
/// registry when the step runs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PipeTarget {
    Name(String),
    Var(String),
}

/// Value-model re-exports: the word, its payload, type descriptors, and the
/// export hook live in [`crate::value`], the single representation for
/// every type.
pub use crate::value::{
    OxDockType, TypeDescriptor, Value, ValuePayload, clone_boxed, clone_copy, clone_shared,
    drop_boxed, drop_noop, drop_shared, eq_boxed, eq_inline, eq_shared, fmt_boxed, fmt_inline,
    fmt_shared, load_inline, startup_descriptors, store_inline, type_anchor, unshare_boxed,
    unshare_inline, unshare_shared,
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Flat stack-machine op for expression-local arithmetic/comparison.
///
/// Lowering folds constant subtrees to `Expr::Literal` and compiles dynamic
/// arithmetic/comparison subtrees to post-order `Vec<MathOp>` so the runtime
/// executes a single instruction loop instead of recursive `Box` walking.
/// `Call` covers value-semantics functions only (`INT`, `FLOAT`, `GLOB`,
/// `LOAD_TOML`, `LOAD_JSON`); `INSPECT($var)` uses `Inspect` to preserve the
/// variable identifier (pre-evaluating to `Value` would lose the name).
#[derive(Debug, Clone, PartialEq)]
pub enum MathOp {
    PushConst(Value),
    LoadVar(String),
    LoadEnv(String),
    LoadKeyPath { base: String, keys: Vec<String> },
    Call { name: String, arity: usize },
    Inspect(String),
    Neg,
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LogicalOp {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Var(String),
    /// Environment read (`env:KEY`): resolves against the script
    /// environment at evaluation time.
    Env(String),
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
    /// Variable inspection (`INSPECT($var)`): carries the variable name
    /// unevaluated so evaluation can snapshot the binding. Produced only by
    /// the parser for the exact `INSPECT` name; evaluators match on this
    /// variant and never on a function-name string.
    Inspect(String),
    Compare {
        op: CompareOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Arithmetic {
        op: ArithOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// Lowering-optimized form: folded literals stay `Literal`, dynamic
    /// arithmetic/comparison subtrees arrive here as flat RPN.
    CompiledMath(Vec<MathOp>),
    /// Lowering-only intermediate staging `9223372036854775808` (the unsigned
    /// half of `i64::MIN`). Valid only as the direct child of unary `-`;
    /// any instance reaching lowering completion bails integer overflow.
    UnsignedIntBoundary(u64),
    Not(Box<Expr>),
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
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

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(v) => write!(f, "{}", v),
            Expr::Var(name) => write!(f, "${}", name),
            Expr::Env(key) => write!(f, "env:{}", key),
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
            Expr::Inspect(var) => write!(f, "INSPECT(${})", var),
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
            Expr::Arithmetic { op, left, right } => {
                write!(f, "({} {} {})", left, op, right)
            }
            Expr::CompiledMath(ops) => {
                write!(f, "{}", format_compiled_math(ops))
            }
            Expr::UnsignedIntBoundary(n) => write!(f, "{}", n),
            Expr::Not(inner) => {
                // Parenthesize compound operands so Display round-trips:
                // `!(a == b)` must not render as `!a == b` (= `(!a) == b`).
                match inner.as_ref() {
                    Expr::Compare { .. } | Expr::Arithmetic { .. } | Expr::CompiledMath(_) => {
                        write!(f, "!({})", inner)
                    }
                    _ => write!(f, "!{}", inner),
                }
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
            CompareOp::Lt => write!(f, "<"),
            CompareOp::Le => write!(f, "<="),
            CompareOp::Gt => write!(f, ">"),
            CompareOp::Ge => write!(f, ">="),
        }
    }
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArithOp::Add => write!(f, "+"),
            ArithOp::Sub => write!(f, "-"),
            ArithOp::Mul => write!(f, "*"),
            ArithOp::Div => write!(f, "/"),
        }
    }
}

/// Render flat RPN back to parenthesized infix so `Display` round-trips
/// through the parser with identical semantics. Parentheses are emitted
/// unconditionally around binary/unary ops; redundant parens parse to the
/// same tree, which is what round-trip requires.
fn format_compiled_math(ops: &[MathOp]) -> String {
    let mut stack: Vec<String> = Vec::new();
    for op in ops {
        match op {
            MathOp::PushConst(v) => stack.push(format!("{}", v)),
            MathOp::LoadVar(name) => stack.push(format!("${}", name)),
            MathOp::LoadEnv(key) => stack.push(format!("env:{}", key)),
            MathOp::LoadKeyPath { base, keys } => {
                let mut s = format!("${}", base);
                for key in keys {
                    s.push('.');
                    s.push_str(key);
                }
                stack.push(s);
            }
            MathOp::Call { name, arity } => {
                let mut args = Vec::new();
                for _ in 0..*arity {
                    args.push(stack.pop().unwrap_or_else(|| "<underflow>".to_string()));
                }
                args.reverse();
                stack.push(format!("{}({})", name, args.join(", ")));
            }
            MathOp::Inspect(name) => stack.push(format!("INSPECT(${})", name)),
            MathOp::Neg => {
                let inner = stack.pop().unwrap_or_else(|| "<underflow>".to_string());
                stack.push(format!("(-{})", inner));
            }
            MathOp::Add => push_bin(&mut stack, "+"),
            MathOp::Sub => push_bin(&mut stack, "-"),
            MathOp::Mul => push_bin(&mut stack, "*"),
            MathOp::Div => push_bin(&mut stack, "/"),
            MathOp::Lt => push_bin(&mut stack, "<"),
            MathOp::Le => push_bin(&mut stack, "<="),
            MathOp::Gt => push_bin(&mut stack, ">"),
            MathOp::Ge => push_bin(&mut stack, ">="),
            MathOp::Eq => push_bin(&mut stack, "=="),
            MathOp::Ne => push_bin(&mut stack, "!="),
        }
    }
    if stack.len() == 1 {
        let mut items = stack;
        items.pop().unwrap_or_else(|| "<empty>".to_string())
    } else {
        stack.join(" ")
    }
}

fn push_bin(stack: &mut Vec<String>, op: &str) {
    let right = stack.pop().unwrap_or_else(|| "<underflow>".to_string());
    let left = stack.pop().unwrap_or_else(|| "<underflow>".to_string());
    stack.push(format!("({} {} {})", left, op, right));
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

#[cfg(test)]
mod tests {
    use super::*;
    /// Bidirectional lock between `dsl.pest` and the keyword registry.
    /// Fails CI if a statement keyword is renamed or deleted on either side.
    /// The grammar is parsed with `pest_meta` (no line-format or rule-name
    /// conventions): every [`STRUCTURAL_KEYWORDS`] entry must occur as a
    /// string literal somewhere in the grammar, and every uppercase literal
    /// reachable from the top-level instruction rules (`element`,
    /// `block_element`, following rule references transitively) must
    /// classify as a statement starter ([`Command::is_statement_keyword`])
    /// or a clause keyword ([`CLAUSE_KEYWORDS`]). Type tags, argument
    /// enums, and value-level function names validate at lowering time via
    /// generic ident rules, so they are outside this test's scope by design
    /// rather than via synthetic registries.
    #[test]
    fn statement_keywords_match_pest_grammar() {
        use pest_meta::{ast::Expr, parser};
        use std::collections::{HashMap, HashSet};

        let pest_src = include_str!("dsl.pest");
        let pairs = parser::parse(parser::Rule::grammar_rules, pest_src)
            .expect("dsl.pest must parse as a pest grammar");
        let rules = parser::consume_rules(pairs).expect("dsl.pest rules must consume");
        let by_name: HashMap<&str, &Expr> = rules
            .iter()
            .map(|rule| (rule.name.as_str(), &rule.expr))
            .collect();

        let mut all_literals = HashSet::new();
        for rule in &rules {
            for node in rule.expr.iter_top_down() {
                match node {
                    Expr::Str(literal) | Expr::Insens(literal) => {
                        all_literals.insert(literal.clone());
                    }
                    _ => {}
                }
            }
        }
        for kw in STRUCTURAL_KEYWORDS {
            assert!(
                all_literals.contains(*kw),
                "keyword {kw} in STRUCTURAL_KEYWORDS missing from dsl.pest"
            );
        }

        let mut seen = HashSet::new();
        let mut stack = vec!["element".to_string(), "block_element".to_string()];
        let mut stmt_literals = HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(expr) = by_name.get(name.as_str()) else {
                continue;
            };
            for node in expr.iter_top_down() {
                match node {
                    Expr::Str(literal) | Expr::Insens(literal) => {
                        stmt_literals.insert(literal.clone());
                    }
                    Expr::Ident(dependency) => stack.push(dependency.clone()),
                    _ => {}
                }
            }
        }
        assert!(
            seen.contains("element") && seen.contains("block_element"),
            "grammar must define element and block_element instruction rules"
        );
        for kw in stmt_literals {
            if kw.len() > 1 && kw.chars().all(|c| c.is_ascii_uppercase()) {
                assert!(
                    crate::Command::is_statement_keyword(&kw)
                        || CLAUSE_KEYWORDS.contains(&kw.as_str()),
                    "uppercase literal \"{kw}\" reachable from dsl.pest instruction rules is not a registered statement or clause keyword"
                );
            }
        }
    }
}
