use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

pub use crate::commands::{AssertTarget, StepKind};
use crate::constants::{
    KEYWORD_ASYNC, KEYWORD_AWAIT, KEYWORD_BREAK, KEYWORD_CANCEL, KEYWORD_CONTINUE, KEYWORD_ELSE,
    KEYWORD_EXPORT, KEYWORD_FOR, KEYWORD_FUNC, KEYWORD_IF, KEYWORD_IMPORT, KEYWORD_LET,
    KEYWORD_REMOTE, KEYWORD_RETURN, KEYWORD_TYPE, KEYWORD_WHILE,
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

/// Core command vocabulary: every statement keyword the grammar accepts.
/// Multi-word commands group by domain prefix (`CATEGORY_ACTION`):
/// `LIST_APPEND`, `READ_LINE`, `ASSERT_EQ`, `COPY_GIT`. The first
/// underscore-separated segment is the category; introducing a command
/// under a new category means adding its prefix to
/// `COMMAND_CATEGORIES` below, so new groupings stay deliberate.
/// Single-word commands carry no category. Frozen names never change.
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
    ListAppend,
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
    Command::ListAppend,
];

/// Registered multi-word command categories (first underscore-separated
/// segment). A new `CATEGORY_ACTION` command registers its prefix here;
/// reusing an existing category needs no change. Single-word commands
/// carry no category and are unaffected.
pub const COMMAND_CATEGORIES: &[&str] =
    &["INHERIT", "WITH", "COPY", "HASH", "READ", "ASSERT", "LIST"];

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
            Command::ListAppend => "LIST_APPEND",
        }
    }

    pub const fn syntax(self) -> &'static str {
        match self {
            Command::InheritEnv => "INHERIT_ENV [KEY1, KEY2, ...]",
            Command::Workdir => "WORKDIR <path>",
            Command::Workspace => "WORKSPACE SNAPSHOT|LOCAL|CACHE|SYSTEM [--local]",
            Command::Env => "ENV KEY=value",
            Command::Echo => "ECHO <message>",
            Command::Run => "RUN <command...> | RUN [\"exe\", \"arg\", ...]",
            Command::Copy => "COPY [--from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM] <from> <to>",
            Command::CopyGit => "COPY_GIT [--include-dirty] <rev> <src> <dst>",
            Command::WithIo => "WITH_IO [bindings] [command | { block }]",
            Command::HashSha256 => "HASH_SHA256 <path>",
            Command::Symlink => {
                "SYMLINK [--from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM] <from> <to>"
            }
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
            Command::ListAppend => "LIST_APPEND $list <item>",
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
            "LIST_APPEND" => Some(Command::ListAppend),
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
    KEYWORD_REMOTE,
    KEYWORD_IMPORT,
    KEYWORD_EXPORT,
    KEYWORD_TYPE,
];

/// Clause keywords that open no statement and need no `Display` quoting
/// (`IN` only heads `FOR` iterations). Declared alongside
/// [`STRUCTURAL_KEYWORDS`] so grammar-conformance tests never hardcode
/// exception lists of their own.
pub const CLAUSE_KEYWORDS: &[&str] = &["IN"];

/// One namespaced guard predicate. Every guard in the language is a
/// namespace plus an optional subject plus an optional value; the
/// single disjointness rule compares the whole triple, so no domain
/// ever needs its own table.
///
/// Construction (fixed by the parser, never hand-built):
/// `os`/`bool` carry the value (`[os:linux]`), `env` carries the
/// variable as subject (`[env:MODE]`, `[env:MODE=dev]`).
/// `family:unix` and `family:windows` are accepted aliases lowered
/// at parse to `any(os:macos, os:linux)` and `os:windows`
/// respectively, so every platform check executes uniformly under
/// the `os:` namespace.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Guard {
    Attr {
        ns: Ns,
        key: Option<String>,
        val: Option<String>,
    },
}

/// Guard namespaces: the closed set the parser accepts. Adding a
/// namespace forces compile errors at every consumer (parse,
/// display, evaluation) — never a silent `_` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ns {
    Os,
    Arch,
    Bool,
    Env,
}

impl fmt::Display for Ns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ns::Os => write!(f, "os"),
            Ns::Arch => write!(f, "arch"),
            Ns::Bool => write!(f, "bool"),
            Ns::Env => write!(f, "env"),
        }
    }
}

/// Every documented `std::env::consts::ARCH` value: the closed
/// `arch:` domain. Shared by parser validation and coverage
/// reasoning so the two can never disagree on membership.
pub const ARCH_VALUES: &[&str] = &[
    "aarch64",
    "arm",
    "arm64ec",
    "avr",
    "bpf",
    "csky",
    "hexagon",
    "loongarch32",
    "loongarch64",
    "mips",
    "mips32r6",
    "mips64",
    "mips64r6",
    "msp430",
    "nvptx64",
    "powerpc",
    "powerpc64",
    "riscv32",
    "riscv64",
    "s390x",
    "sparc",
    "sparc64",
    "wasm32",
    "wasm64",
    "x86",
    "x86_64",
    "xtensa",
];

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

/// A command argument : either an expandable string or an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// Expandable string. The `bool` indicates whether the argument was
    /// quoted in the source (`true`) or unquoted (`false`). Quoted arguments
    /// that start with `--` are positional, not flags.
    String(String, bool),
    /// Expression : resolved at runtime via evaluate_expr.
    Expr(Expr),
    /// Mixed literal/expression value (e.g. `KEY={{ $x }} tail`). Fragments
    /// resolve independently at runtime and concatenate with no added
    /// separator : inter-fragment gaps are already materialized as `Text`.
    Parts(Vec<ArgPart>),
}

/// One fragment of a mixed [`Arg::Parts`] value.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgPart {
    /// Literal text. The `bool` marks source-quoted regions (exact bytes);
    /// unquoted text carries single-space-normalized gaps.
    Text(String, bool),
    /// Typed expression : resolved via evaluate_expr, never stringified.
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

/// A pipe endpoint for a `WITH_IO` binding: a `$var` holding a `PIPE`
/// value, resolved against the live variable scope when the step runs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PipeTarget {
    Var(String),
}

/// Value-model re-exports: the word, its payload, type descriptors, and the
/// export hook live in [`crate::value`], the single representation for
/// every type.
pub use crate::value::{
    OxDockType, SemaphoreState, TypeDescriptor, Value, ValuePayload, clone_boxed, clone_copy,
    clone_shared, drop_boxed, drop_noop, drop_shared, eq_boxed, eq_inline, eq_shared, fmt_boxed,
    fmt_inline, fmt_shared, load_inline, startup_descriptors, store_inline, type_anchor,
    unshare_boxed, unshare_inline, unshare_shared,
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
    /// Inline block (`LET $a: STRING = { RETURN "hi" }`): runs its steps in
    /// a fresh scope when evaluated and yields the `RETURN` value
    /// (fallthrough yields `""`, mirroring a zero-arg function body).
    /// Parsed only where `map_literal` fails, so `{k: v}` stays a map.
    Block(Vec<Step>),
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
    /// Fresh anonymous pipe backend (`LET $p: PIPE` with no initializer).
    /// Evaluates to a pipe value keyed by a generated name that no
    /// `pipe:` literal can spell, so bare declarations never collide
    /// with named pipes. Transitional representation until backends
    /// become owned handles.
    FreshPipe,
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

impl Expr {
    /// Visit this expression and every descendant, depth first. `Block`
    /// steps recurse through [`StepKind::walk_step_exprs`](crate::commands::StepKind::walk_step_exprs),
    /// so inline bodies are covered; `CompiledMath` ops are opaque here
    /// (match `MathOp::Call` in the visitor when call names matter).
    /// Exhaustive over variants: adding an `Expr` shape fails compilation
    /// here, never silently.
    pub fn walk(&self, f: &mut impl FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_)
            | Expr::Var(_)
            | Expr::Env(_)
            | Expr::KeyPath { .. }
            | Expr::Inspect(_)
            | Expr::FreshPipe
            | Expr::UnsignedIntBoundary(_) => {}
            Expr::Call { args, .. } => {
                for arg in args {
                    arg.walk(f);
                }
            }
            Expr::List(items) => {
                for item in items {
                    item.walk(f);
                }
            }
            Expr::Map(entries) => {
                for (_, value) in entries {
                    value.walk(f);
                }
            }
            Expr::Block(steps) => {
                for step in steps {
                    step.kind.walk_step_exprs(f);
                }
            }
            Expr::Compare { left, right, .. }
            | Expr::Arithmetic { left, right, .. }
            | Expr::Logical { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::CompiledMath(_) => {}
            Expr::Not(inner) => inner.walk(f),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Snapshot,
    Local,
    Cache { local: bool },
    System,
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
    let Guard::Attr { ns, key, val } = guard;
    match ns {
        Ns::Os => match val.as_deref() {
            Some("macos") => platform_is_macos(),
            Some("linux") => platform_is_linux(),
            Some("windows") => platform_is_windows(),
            _ => false,
        },
        Ns::Arch => match val.as_deref() {
            Some(want) => ARCH_TARGET == want,
            None => false,
        },
        Ns::Bool => val
            .as_deref()
            .map(|v| v.parse::<bool>().unwrap_or(false))
            .unwrap_or(false),
        Ns::Env => match key.as_deref() {
            Some(var) => match val {
                None => env.get_env(var).map(|v| !v.is_empty()).unwrap_or(false),
                Some(want) => env
                    .get_env(var)
                    .map(|v| v == want.as_str())
                    .unwrap_or(false),
            },
            // Parser always supplies the variable; defensive false.
            None => false,
        },
    }
}

/// One `cfg!` per platform fact, each beside its namespace arm
/// above. `#[allow]` lives on the helpers, not the dispatch.
#[allow(clippy::disallowed_macros)]
fn platform_is_windows() -> bool {
    cfg!(windows)
}

/// See `platform_is_windows`.
#[allow(clippy::disallowed_macros)]
fn platform_is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// See `platform_is_windows`.
#[allow(clippy::disallowed_macros)]
fn platform_is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// Host target architecture behind `arch:` guards. A plain const
/// read alongside the platform helpers above.
const ARCH_TARGET: &str = std::env::consts::ARCH;

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

impl Guard {
    /// The one disjointness rule for the entire guard language:
    /// same namespace, same subject, both valued, different values.
    /// Everything else overlaps — different namespaces are
    /// independent axes, different subjects are independent
    /// variables, and a missing value is a presence claim compatible
    /// with any value. No per-domain tables: `os:macos` vs
    /// `os:linux`, `env:MODE=dev` vs `env:MODE=prod`, and
    /// `bool:true` vs `bool:false` all decide here.
    pub fn is_disjoint_from(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Guard::Attr {
                    ns: n1,
                    key: k1,
                    val: Some(v1),
                },
                Guard::Attr {
                    ns: n2,
                    key: k2,
                    val: Some(v2),
                },
            ) => n1 == n2 && k1 == k2 && v1 != v2,
            _ => false,
        }
    }
}

impl GuardExpr {
    /// Structural disjointness over the full combinator language.
    /// Distribution is exact in both directions (`Or` needs every
    /// arm disjoint, `All` needs one), so this proves rather than
    /// assumes; anything without a proof overlaps. Linear, no worlds,
    /// no enumeration, no cap.
    pub fn is_disjoint_from(&self, other: &Self) -> bool {
        match (self, other) {
            // Complements, either polarity: exact syntactic equality,
            // or an unset negation against any positive claim on the
            // same subject (`Not([ns:k])` excludes `[ns:k]` and
            // `[ns:k=v]` alike — unset is compatible with nothing
            // valued). Valued negations (`Not([ns:k=v])`) meet only
            // their exact complement: the subject may hold some other
            // value.
            (x, GuardExpr::Not(inner)) | (GuardExpr::Not(inner), x) => {
                x == inner.as_ref() || unset_excludes(x, inner.as_ref())
            }
            // Leaf predicates delegate to domain algebra.
            (GuardExpr::Predicate(p1), GuardExpr::Predicate(p2)) => p1.is_disjoint_from(p2),
            // Distribution over `Or`: every arm must exclude `other`.
            (GuardExpr::Or(arms), other) | (other, GuardExpr::Or(arms)) => {
                arms.iter().all(|arm| arm.is_disjoint_from(other))
            }
            // Distribution over `All`: one excluding arm suffices.
            (GuardExpr::All(arms), other) | (other, GuardExpr::All(arms)) => {
                arms.iter().any(|arm| arm.is_disjoint_from(other))
            }
        }
    }
}

/// Top-level conjuncts of a guard: descend `All` only. The DNF
/// expansion below distributes anything nested (`Or`/`Not`)
/// exactly; this split is only the entry point.
fn conjuncts(expr: &GuardExpr) -> Vec<&GuardExpr> {
    match expr {
        GuardExpr::All(items) => items.iter().collect(),
        single => vec![single],
    }
}

/// Closed value domains for coverage reasoning, driven by the
/// parser's accepted values rather than per-value match arms in the
/// analyzer: `bool` admits true/false, `os` admits the three known
/// systems (`family:unix` already lowers to `any(os:macos,
/// os:linux)`, so unix-family coverage is structural). `env` is
/// open (any string), so it never exhausts.
fn closed_domain_values(ns: Ns, key: Option<&str>) -> Option<&'static [&'static str]> {
    match (ns, key) {
        (Ns::Bool, None) => Some(&["true", "false"]),
        (Ns::Os, None) => Some(&["macos", "linux", "windows"]),
        (Ns::Arch, None) => Some(ARCH_VALUES),
        _ => None,
    }
}

/// One DNF literal: a predicate or its negation. Negations only
/// ever sit on predicates after NNF pushdown.
#[derive(Clone)]
enum DnfLit {
    Pos(Guard),
    Neg(Guard),
}

impl DnfLit {
    fn as_expr(&self) -> GuardExpr {
        match self {
            DnfLit::Pos(guard) => GuardExpr::Predicate(guard.clone()),
            DnfLit::Neg(guard) => GuardExpr::Not(Box::new(GuardExpr::Predicate(guard.clone()))),
        }
    }
}

/// Cap on DNF disjuncts: guard formulas are a handful of atoms, so
/// this never fires on real input. Exceeding it falls back to live
/// (satisfiable), the sound direction — a missed proof only risks a
/// false coverage error, never a missed one.
const MAX_DNF_DISJUNCTS: usize = 64;

/// Push one expression into DNF under a polarity: `true` keeps it,
/// `false` negates it (De Morgan through `All`/`Or`, double
/// negation cancels). Returns the disjunct list, or `None` when the
/// cap trips.
fn dnf_push(expr: &GuardExpr, polarity: bool, out: &mut Vec<Vec<DnfLit>>) -> bool {
    match (expr, polarity) {
        (GuardExpr::Predicate(guard), true) => {
            for disjunct in out.iter_mut() {
                disjunct.push(DnfLit::Pos(guard.clone()));
            }
            true
        }
        (GuardExpr::Predicate(guard), false) => {
            for disjunct in out.iter_mut() {
                disjunct.push(DnfLit::Neg(guard.clone()));
            }
            true
        }
        (GuardExpr::All(children), true) | (GuardExpr::Or(children), false) => {
            // Conjunction: every child constrains every disjunct.
            for child in children {
                if !dnf_push(child, polarity, out) {
                    return false;
                }
            }
            true
        }
        (GuardExpr::Or(children), true) | (GuardExpr::All(children), false) => {
            // Disjunction: each child forks every live disjunct.
            let mut forked = Vec::new();
            for disjunct in out.drain(..) {
                for child in children {
                    let mut branch = vec![disjunct.clone()];
                    if !dnf_push(child, polarity, &mut branch) {
                        return false;
                    }
                    forked.extend(branch);
                    if forked.len() > MAX_DNF_DISJUNCTS {
                        return false;
                    }
                }
            }
            *out = forked;
            true
        }
        (GuardExpr::Not(inner), _) => dnf_push(inner, !polarity, out),
    }
}

/// True when one DNF disjunct can fire: no statically-false
/// literal, no conflicting pair, and no closed domain exhausted by
/// negations.
fn disjunct_satisfiable(lits: &[DnfLit]) -> bool {
    // A statically-false positive (`bool:false`, or any non-`true`
    // which `guard_allows` parses as false) kills the disjunct, as
    // does the negation of `bool:true` (which always fires).
    for lit in lits {
        match lit {
            DnfLit::Pos(Guard::Attr {
                ns: Ns::Bool, val, ..
            }) if val.as_deref() != Some("true") => return false,
            DnfLit::Neg(Guard::Attr {
                ns: Ns::Bool,
                key: None,
                val: Some(value),
            }) if value == "true" => return false,
            _ => {}
        }
    }
    // Any conflicting pair kills the disjunct (complements, leaf
    // disjointness, and `Or`/`All` distribution decide inside).
    for (index, first) in lits.iter().enumerate() {
        for second in &lits[index + 1..] {
            if first.as_expr().is_disjoint_from(&second.as_expr()) {
                return false;
            }
        }
    }
    // A closed domain with every value negated has nowhere left to
    // fire (`not(os:macos)`, `not(os:linux)`, `not(os:windows)`
    // jointly cover no host).
    for lit in lits {
        let (ns, key) = match lit {
            DnfLit::Neg(Guard::Attr {
                ns,
                key,
                val: Some(_),
            }) => (*ns, key.as_deref()),
            _ => continue,
        };
        let Some(domain) = closed_domain_values(ns, key) else {
            continue;
        };
        let exhausted = domain.iter().all(|value| {
            lits.iter().any(|other| match other {
                DnfLit::Neg(Guard::Attr {
                    ns: other_ns,
                    key: other_key,
                    val: Some(other_val),
                }) => *other_ns == ns && other_key.as_deref() == key && other_val == value,
                _ => false,
            })
        });
        if exhausted {
            return false;
        }
    }
    true
}

/// True when a conjunction list can never fire: its exact DNF has
/// no satisfiable disjunct. This proves rather than assumes —
/// pairwise conflicts, `bool` falsehood, and complete closed-domain
/// partitions (`os:macos`/`os:linux`/`os:windows`,
/// `bool:true`/`bool:false`) all decide here with no per-domain
/// tables and no hardcoded platform pairs.
fn conjunction_dead(atoms: &[&GuardExpr]) -> bool {
    if atoms.is_empty() {
        return false;
    }
    let mut disjuncts = vec![Vec::new()];
    for atom in atoms {
        if !dnf_push(atom, true, &mut disjuncts) {
            return false;
        }
    }
    !disjuncts
        .iter()
        .any(|disjunct| disjunct_satisfiable(disjunct))
}

/// True when a guard can fire. Anything not provably dead is live.
pub fn guard_satisfiable(expr: &GuardExpr) -> bool {
    !conjunction_dead(&conjuncts(expr))
}

/// True when a guard fires unconditionally: a lone `true`, an
/// all-valid conjunction, or the negation of a dead conjunction.
pub fn guard_valid(expr: &GuardExpr) -> bool {
    match expr {
        GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Bool,
            val: Some(value),
            ..
        }) => value.as_str() == "true",
        GuardExpr::All(items) => items.iter().all(guard_valid),
        GuardExpr::Not(inner) => !guard_satisfiable(inner),
        _ => false,
    }
}

/// True when two declaration guards can fire together. Absent guards
/// are unconditional; anything without a provable conflict overlaps.
pub fn guards_overlap(a: Option<&GuardExpr>, b: Option<&GuardExpr>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), None) | (None, Some(x)) => guard_satisfiable(x),
        (Some(x), Some(y)) => {
            let mut atoms = conjuncts(x);
            atoms.extend(conjuncts(y));
            !conjunction_dead(&atoms)
        }
    }
}

/// Unset exclusion: `Not([ns:k])` (valueless) is disjoint from any
/// positive claim on the same subject, valued or not.
fn unset_excludes(x: &GuardExpr, inner: &GuardExpr) -> bool {
    match (x, inner) {
        (
            GuardExpr::Predicate(Guard::Attr {
                ns: n2, key: k2, ..
            }),
            GuardExpr::Predicate(Guard::Attr {
                ns: n1,
                key: k1,
                val: None,
            }),
        ) => n1 == n2 && k1 == k2,
        _ => false,
    }
}

impl fmt::Display for Guard {
    /// Bare form without brackets (the step renderer adds them):
    /// `[os:linux]` prints `os:linux`, `[env:MODE=dev]`
    /// prints `eq(env:MODE, dev)`, `[env:MODE]` prints `env:MODE`.
    /// The `eq()` spelling roundtrips through the parser; the rest
    /// print verbatim. Roundtrips by construction.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Guard::Attr {
                ns: Ns::Env,
                key: Some(var),
                val: None,
            } => write!(f, "env:{var}"),
            Guard::Attr {
                ns: Ns::Env,
                key: Some(var),
                val: Some(want),
            } => write!(f, "eq(env:{var}, {want})"),
            Guard::Attr {
                ns,
                key: _,
                val: Some(value),
            } => write!(f, "{ns}:{value}"),
            Guard::Attr { ns, key, val: None } => match key {
                Some(subject) => write!(f, "{ns}:{subject}"),
                None => write!(f, "{ns}"),
            },
        }
    }
}

impl fmt::Display for WorkspaceTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceTarget::Snapshot => write!(f, "SNAPSHOT"),
            WorkspaceTarget::Local => write!(f, "LOCAL"),
            WorkspaceTarget::Cache { local: false } => write!(f, "CACHE"),
            WorkspaceTarget::Cache { local: true } => write!(f, "CACHE --local"),
            WorkspaceTarget::System => write!(f, "SYSTEM"),
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
            Expr::Block(steps) => {
                write!(f, "{{ ")?;
                for (i, step) in steps.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{}", step.kind)?;
                }
                write!(f, " }}")
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
            // No expression syntax produces this (bare `LET` is a
            // statement): render loudly non-round-trippable so a stray
            // use fails at re-parse instead of aliasing a named pipe.
            Expr::FreshPipe => write!(f, "<fresh pipe>"),
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

    /// Category convention lock: every multi-word command groups under a
    /// registered `COMMAND_CATEGORIES` prefix. Adding a command under a
    /// new category fails here until the prefix registers (one line).
    #[test]
    fn command_names_group_by_category() {
        for command in COMMANDS {
            let name = command.as_str();
            let Some((prefix, _)) = name.split_once('_') else {
                continue;
            };
            assert!(
                COMMAND_CATEGORIES.contains(&prefix),
                "command {name} introduces unregistered category {prefix}; add it to COMMAND_CATEGORIES"
            );
        }
    }
}

#[cfg(test)]
mod guard_analysis_tests {
    use super::*;

    fn attr(ns: Ns, key: Option<&str>, val: Option<&str>) -> GuardExpr {
        GuardExpr::Predicate(Guard::Attr {
            ns,
            key: key.map(str::to_string),
            val: val.map(str::to_string),
        })
    }

    fn os(value: &str) -> GuardExpr {
        attr(Ns::Os, None, Some(value))
    }

    fn arch(value: &str) -> GuardExpr {
        attr(Ns::Arch, None, Some(value))
    }

    /// The `family:unix` parse alias, desugared exactly as the
    /// parser lowers it.
    fn unix() -> GuardExpr {
        GuardExpr::Or(vec![os("macos"), os("linux")])
    }

    fn windows() -> GuardExpr {
        os("windows")
    }

    fn env_eq(var: &str, value: &str) -> GuardExpr {
        attr(Ns::Env, Some(var), Some(value))
    }

    fn not(expr: GuardExpr) -> GuardExpr {
        GuardExpr::Not(Box::new(expr))
    }

    /// Closed domains exhaust: negating every `os:` value (or both
    /// `bool:` values) covers no host. Open domains (`env:`) never
    /// exhaust — some unmentioned value may hold.
    #[test]
    fn partition_negations_are_jointly_dead() {
        let os_exhausted = GuardExpr::All(vec![not(os("macos")), not(os("linux")), not(windows())]);
        assert!(
            !guard_satisfiable(&os_exhausted),
            "no host is outside macos/linux/windows"
        );
        // Partial negations stay live.
        let os_partial = GuardExpr::All(vec![not(os("macos")), not(os("linux"))]);
        assert!(guard_satisfiable(&os_partial), "windows is still open");
        // The bool domain exhausts the same way.
        let bool_exhausted = GuardExpr::All(vec![
            not(attr(Ns::Bool, None, Some("true"))),
            not(attr(Ns::Bool, None, Some("false"))),
        ]);
        assert!(!guard_satisfiable(&bool_exhausted));
        // Open env domains never exhaust.
        let env_open = GuardExpr::All(vec![
            not(env_eq("MODE", "dev")),
            not(env_eq("MODE", "prod")),
        ]);
        assert!(guard_satisfiable(&env_open), "MODE may hold a third value");
        // Single negations are always live.
        assert!(guard_satisfiable(&not(unix())));
    }

    /// The desugared unix alias behaves as a disjunction: it meets
    /// windows nowhere and covers macos/linux deployment reads.
    #[test]
    fn unix_alias_is_os_disjunction() {
        assert!(!guards_overlap(Some(&unix()), Some(&windows())));
        assert!(guards_overlap(Some(&unix()), Some(&os("macos"))));
        // Coverage through the alias: unix + windows declarations
        // cover every host, so their joint negation is dead.
        let covered = GuardExpr::All(vec![GuardExpr::Not(Box::new(unix())), not(windows())]);
        assert!(!guard_satisfiable(&covered));
    }

    /// `arch:` decides by value like every namespace; it overlaps
    /// `os:` (independent axes: an aarch64 mac exists). Negating
    /// the whole closed `ARCH_VALUES` domain covers no host.
    #[test]
    fn arch_guards_decide_by_value() {
        assert!(!guards_overlap(
            Some(&arch("x86_64")),
            Some(&arch("aarch64"))
        ));
        assert!(guards_overlap(Some(&arch("x86_64")), Some(&arch("x86_64"))));
        assert!(guards_overlap(Some(&arch("aarch64")), Some(&os("macos"))));
        let exhausted = GuardExpr::All(ARCH_VALUES.iter().map(|value| not(arch(value))).collect());
        assert!(!guard_satisfiable(&exhausted));
        // Runtime anchor: this host's own architecture fires here.
        let host = attr(Ns::Arch, None, Some(ARCH_TARGET));
        assert!(guard_expr_allows(&host, &HashMap::new()));
    }

    /// The full platform matrix, asserted exhaustively. All three
    /// `os:` values are pairwise disjoint; identical values
    /// overlap.
    #[test]
    fn platform_overlap_matrix_is_exhaustive() {
        let cases = [
            ("macos", "macos", true),
            ("macos", "linux", false),
            ("macos", "windows", false),
            ("linux", "linux", true),
            ("linux", "windows", false),
            ("windows", "windows", true),
        ];
        for (v1, v2, expected) in cases {
            let left = os(v1);
            let right = os(v2);
            assert_eq!(
                guards_overlap(Some(&left), Some(&right)),
                expected,
                "os:{v1} vs os:{v2}"
            );
        }
    }

    /// Runtime anchor, portable across CI hosts: `os:macos` and
    /// `os:windows` never co-fire on the machine running the test.
    #[test]
    fn disjoint_pairs_never_cofire_here() {
        let macos = os("macos");
        let holds = |expr: &GuardExpr| guard_expr_allows(expr, &HashMap::new());
        assert!(
            !(holds(&macos) && holds(&windows())),
            "os:macos and os:windows co-fired"
        );
    }

    #[test]
    fn overlap_follows_namespace_semantics() {
        // Same namespace, distinct values: disjoint.
        assert!(!guards_overlap(Some(&windows()), Some(&unix())));
        assert!(!guards_overlap(Some(&os("macos")), Some(&os("linux"))));
        // Absent guards are unconditional.
        assert!(guards_overlap(None, Some(&windows())));
        assert!(guards_overlap(None, None));
        // Unsatisfiable conjunctions overlap nothing.
        let dead = GuardExpr::All(vec![os("macos"), windows()]);
        assert!(!guard_satisfiable(&dead));
        assert!(!guards_overlap(Some(&dead), Some(&os("macos"))));
        assert!(guard_valid(&GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Bool,
            key: None,
            val: Some("true".to_string()),
        })));
        assert!(!guard_valid(&unix()));
    }

    #[test]
    fn env_value_conflicts_decide_without_enumeration() {
        let fast = env_eq("MODE", "fast");
        let slow = env_eq("MODE", "slow");
        assert!(
            !guards_overlap(Some(&fast), Some(&slow)),
            "same key distinct values are disjoint"
        );
        let also_fast = env_eq("MODE", "fast");
        assert!(guards_overlap(Some(&fast), Some(&also_fast)));
        // Bare presence overlaps any value on the same key.
        let bare = attr(Ns::Env, Some("MODE"), None);
        assert!(guards_overlap(Some(&bare), Some(&fast)));
    }
}

#[cfg(test)]
mod guard_algebra_tests {
    use super::*;

    fn os(value: &str) -> GuardExpr {
        GuardExpr::Predicate(Guard::Attr {
            ns: Ns::Os,
            key: None,
            val: Some(value.to_string()),
        })
    }

    #[test]
    fn or_distributes_over_disjointness() {
        fn env_eq(var: &str, value: &str) -> GuardExpr {
            GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some(var.to_string()),
                val: Some(value.to_string()),
            })
        }
        // Every arm must exclude `other`: one overlapping arm keeps
        // the whole disjunction live.
        let mixed = GuardExpr::Or(vec![env_eq("MODE", "dev"), env_eq("MODE", "prod")]);
        assert!(!mixed.is_disjoint_from(&env_eq("MODE", "dev")));
        // All arms disjoint: the disjunction is dead against it.
        assert!(mixed.is_disjoint_from(&env_eq("MODE", "test")));
        // Cross-namespace arms never exclude: different axes overlap.
        let cross = GuardExpr::Or(vec![env_eq("MODE", "dev")]);
        let platform = os("windows");
        assert!(!cross.is_disjoint_from(&platform));
    }

    #[test]
    fn all_distributes_over_disjointness() {
        let stacked = GuardExpr::All(vec![
            os("macos"),
            GuardExpr::Or(vec![os("macos"), os("linux")]),
            os("windows"),
        ]);
        assert!(stacked.is_disjoint_from(&os("macos")));
        // One live arm keeps the conjunction overlapping: neither
        // arm excludes macos here.
        let live = GuardExpr::All(vec![
            os("macos"),
            GuardExpr::Predicate(Guard::Attr {
                ns: Ns::Env,
                key: Some("MODE".to_string()),
                val: Some("dev".to_string()),
            }),
        ]);
        assert!(!live.is_disjoint_from(&os("macos")));
    }

    #[test]
    fn negation_is_structural_not_syntactic() {
        let not_win = GuardExpr::Not(Box::new(os("windows")));
        assert!(not_win.is_disjoint_from(&os("windows")));
        assert!(os("windows").is_disjoint_from(&not_win));
        assert!(!not_win.is_disjoint_from(&os("macos")));
    }
}
