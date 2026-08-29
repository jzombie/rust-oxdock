use std::collections::HashMap;

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
    RawWrite,
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
    Command::RawWrite,
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
            Command::RawWrite => "RAW_WRITE",
            Command::Append => "APPEND",
            Command::Expand => "EXPAND",
            Command::AssertFile => "ASSERT_FILE",
            Command::AssertDir => "ASSERT_DIR",
            Command::AssertAbsent => "ASSERT_ABSENT",
            Command::AssertStdout => "ASSERT_STDOUT",
            Command::Exit => "EXIT",
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
            "RAW_WRITE" => Some(Command::RawWrite),
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
        invert: bool,
    },
    EnvExists {
        key: String,
        invert: bool,
    },
    EnvEquals {
        key: String,
        value: String,
        invert: bool,
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

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TemplateString(pub String);

impl From<String> for TemplateString {
    fn from(s: String) -> Self {
        TemplateString(s)
    }
}

impl From<&str> for TemplateString {
    fn from(s: &str) -> Self {
        TemplateString(s.to_string())
    }
}

impl std::fmt::Display for TemplateString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for TemplateString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for TemplateString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for TemplateString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl std::ops::Deref for TemplateString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
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
pub enum StepKind {
    Workdir(TemplateString),
    Workspace(WorkspaceTarget),
    Env {
        key: String,
        value: TemplateString,
    },
    /// Directive to inherit a selective list of environment variables from the host.
    /// This is intended to be declared in the prelude/top-level only.
    InheritEnv {
        keys: Vec<String>,
    },
    Run(TemplateString),
    Echo(TemplateString),
    RunBg(TemplateString),
    Copy {
        from_current_workspace: bool,
        from: TemplateString,
        to: TemplateString,
    },
    Symlink {
        from: TemplateString,
        to: TemplateString,
    },
    Mkdir(TemplateString),
    Ls(Option<TemplateString>),
    Cwd,
    Read(Option<TemplateString>),
    Write {
        path: TemplateString,
        contents: Option<TemplateString>,
    },
    /// RAW_WRITE writes literal bytes to a file without expanding template
    /// placeholders.  The path is still resolved via `expand_template`; only
    /// the file contents bypass expansion.
    RawWrite {
        path: TemplateString,
        contents: String,
    },
    Append {
        path: TemplateString,
        contents: Option<TemplateString>,
    },
    /// EXPAND `[path]` `[KEY=val ...]`
    ///
    /// Reads file or stdin, expands `{{ env:KEY }}` placeholders, outputs to stdout.
    /// Explicit KEY=val arguments override env vars.
    Expand {
        path: Option<TemplateString>,
        overrides: Vec<(String, TemplateString)>,
    },
    /// Verify a workspace file exists and, when `hash` or `contents` is
    /// present, matches it. The grammar guarantees the invariant: `hash` set
    /// implies a 64-hex digest and no `contents`.
    AssertFile {
        hash: Option<String>,
        path: TemplateString,
        contents: Option<TemplateString>,
    },
    AssertDir(TemplateString),
    AssertAbsent(TemplateString),
    AssertStdout(TemplateString),
    WithIo {
        bindings: Vec<IoBinding>,
        cmd: Box<StepKind>,
    },
    WithIoBlock {
        bindings: Vec<IoBinding>,
    },
    CopyGit {
        rev: TemplateString,
        from: TemplateString,
        to: TemplateString,
        include_dirty: bool,
    },
    HashSha256 {
        path: TemplateString,
    },
    Exit(i32),
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

pub fn guard_allows(guard: &Guard, script_envs: &HashMap<String, String>) -> bool {
    match guard {
        Guard::Platform { target, invert } => {
            let res = platform_matches(*target);
            if *invert { !res } else { res }
        }
        Guard::EnvExists { key, invert } => {
            let res = script_envs
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if *invert { !res } else { res }
        }
        Guard::EnvEquals { key, value, invert } => {
            let res = script_envs
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .map(|v| v == *value)
                .unwrap_or(false);
            if *invert { !res } else { res }
        }
    }
}

pub fn guard_expr_allows(expr: &GuardExpr, script_envs: &HashMap<String, String>) -> bool {
    match expr {
        GuardExpr::Predicate(guard) => guard_allows(guard, script_envs),
        GuardExpr::All(children) => children.iter().all(|g| guard_expr_allows(g, script_envs)),
        GuardExpr::Or(children) => children.iter().any(|g| guard_expr_allows(g, script_envs)),
        GuardExpr::Not(child) => !guard_expr_allows(child, script_envs),
    }
}

pub fn guard_option_allows(
    expr: Option<&GuardExpr>,
    script_envs: &HashMap<String, String>,
) -> bool {
    match expr {
        Some(e) => guard_expr_allows(e, script_envs),
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
            Guard::Platform { target, invert } => {
                if *invert {
                    write!(f, "!{}", target)
                } else {
                    write!(f, "{}", target)
                }
            }
            Guard::EnvExists { key, invert } => {
                if *invert {
                    write!(f, "!")?
                }
                write!(f, "env:{}", key)
            }
            Guard::EnvEquals { key, value, invert } => {
                if *invert {
                    write!(f, "env:{}!={}", key, value)
                } else {
                    write!(f, "env:{}=={}", key, value)
                }
            }
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
            StepKind::Workdir(arg) => write!(f, "WORKDIR {}", quote_arg(arg)),
            StepKind::Workspace(target) => write!(f, "WORKSPACE {}", target),
            StepKind::Env { key, value } => write!(f, "ENV {}={}", key, quote_arg(value)),
            StepKind::Run(cmd) => write!(f, "RUN {}", quote_run(cmd)),
            StepKind::Echo(msg) => write!(f, "ECHO {}", quote_msg(msg)),
            StepKind::RunBg(cmd) => write!(f, "RUN_BG {}", quote_run(cmd)),
            StepKind::Copy {
                from_current_workspace,
                from,
                to,
            } => {
                if *from_current_workspace {
                    write!(
                        f,
                        "COPY --from-current-workspace {} {}",
                        quote_arg(from),
                        quote_arg(to)
                    )
                } else {
                    write!(f, "COPY {} {}", quote_arg(from), quote_arg(to))
                }
            }
            StepKind::Symlink { from, to } => {
                write!(f, "SYMLINK {} {}", quote_arg(from), quote_arg(to))
            }
            StepKind::Mkdir(arg) => write!(f, "MKDIR {}", quote_arg(arg)),
            StepKind::Ls(arg) => {
                write!(f, "LS")?;
                if let Some(a) = arg {
                    write!(f, " {}", quote_arg(a))?;
                }
                Ok(())
            }
            StepKind::Cwd => write!(f, "CWD"),
            StepKind::Read(arg) => {
                write!(f, "READ")?;
                if let Some(a) = arg {
                    write!(f, " {}", quote_arg(a))?;
                }
                Ok(())
            }
            StepKind::Write { path, contents } => {
                write!(f, "WRITE {}", quote_arg(path))?;
                if let Some(body) = contents {
                    write!(f, " {}", quote_msg(body))?;
                }
                Ok(())
            }
            StepKind::RawWrite { path, contents } => {
                write!(f, "RAW_WRITE {} {}", quote_arg(path), quote_msg(contents))
            }
            StepKind::Append { path, contents } => {
                write!(f, "APPEND {}", quote_arg(path))?;
                if let Some(body) = contents {
                    write!(f, " {}", quote_msg(body))?;
                }
                Ok(())
            }
            StepKind::Expand { path, overrides } => {
                write!(f, "EXPAND")?;
                if let Some(p) = path {
                    write!(f, " {}", quote_arg(p))?;
                }
                for (key, value) in overrides {
                    write!(f, " {}={}", key, quote_arg(value))?;
                }
                Ok(())
            }
            StepKind::AssertFile {
                hash,
                path,
                contents,
            } => {
                if let Some(digest) = hash {
                    write!(f, "ASSERT_FILE --hash {} {}", digest, quote_arg(path))
                } else {
                    write!(f, "ASSERT_FILE {}", quote_arg(path))?;
                    if let Some(body) = contents {
                        write!(f, " {}", quote_msg(body))?;
                    }
                    Ok(())
                }
            }
            StepKind::AssertDir(arg) => write!(f, "ASSERT_DIR {}", quote_arg(arg)),
            StepKind::AssertAbsent(arg) => write!(f, "ASSERT_ABSENT {}", quote_arg(arg)),
            StepKind::AssertStdout(msg) => write!(f, "ASSERT_STDOUT {}", quote_msg(msg)),
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
                        quote_arg(rev),
                        quote_arg(from),
                        quote_arg(to)
                    )
                } else {
                    write!(
                        f,
                        "COPY_GIT {} {} {}",
                        quote_arg(rev),
                        quote_arg(from),
                        quote_arg(to)
                    )
                }
            }
            StepKind::HashSha256 { path } => write!(f, "HASH_SHA256 {}", quote_arg(path)),
            StepKind::Exit(code) => write!(f, "EXIT {}", code),
        }
    }
}

enum GuardDisplayContext {
    Root,
    InOrArg,
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
                    GuardDisplayContext::InOrArg | GuardDisplayContext::InNot
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
                write!(f, "or(")?;
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    child.fmt_with_ctx(f, GuardDisplayContext::InOrArg)?;
                }
                write!(f, ")")
            }
            GuardExpr::Not(child) => {
                write!(f, "!")?;
                let needs_paren =
                    !matches!(child.as_ref(), GuardExpr::Predicate(_) | GuardExpr::Not(_));
                if needs_paren {
                    write!(f, "(")?;
                }
                child.fmt_with_ctx(f, GuardDisplayContext::InNot)?;
                if needs_paren {
                    write!(f, ")")?;
                }
                Ok(())
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
