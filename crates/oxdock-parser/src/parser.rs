use crate::ast::{
    Guard, GuardExpr, IoBinding, IoStream, PlatformGuard, Step, StepKind, WorkspaceTarget,
};
use crate::lexer::{self, RawToken, Rule};
use anyhow::{Result, anyhow, bail};
use pest::iterators::Pair;
use std::collections::VecDeque;

#[derive(Clone)]
struct ScopeFrame {
    line_no: usize,
    had_command: bool,
}

#[derive(Clone)]
struct PendingIoBlock {
    line_no: usize,
    bindings: Vec<IoBinding>,
    guards: Option<GuardExpr>,
}

#[derive(Clone)]
struct IoScopeFrame {
    line_no: usize,
    had_command: bool,
    bindings: Vec<IoBinding>,
    guards: Option<GuardExpr>,
}

#[derive(Clone, Copy)]
enum BlockKind {
    Guard,
    Io,
}

#[derive(Default)]
struct IoBindingSet {
    stdin: Option<IoBinding>,
    stdout: Option<IoBinding>,
    stderr: Option<IoBinding>,
}

impl IoBindingSet {
    fn insert(&mut self, binding: IoBinding) {
        match binding.stream {
            IoStream::Stdin => self.stdin = Some(binding),
            IoStream::Stdout => self.stdout = Some(binding),
            IoStream::Stderr => self.stderr = Some(binding),
        }
    }

    fn into_vec(self) -> Vec<IoBinding> {
        let mut out = Vec::new();
        if let Some(binding) = self.stdin {
            out.push(binding);
        }
        if let Some(binding) = self.stdout {
            out.push(binding);
        }
        if let Some(binding) = self.stderr {
            out.push(binding);
        }
        out
    }
}

pub struct ScriptParser<'a> {
    tokens: VecDeque<RawToken<'a>>,
    steps: Vec<Step>,
    guard_stack: Vec<Option<GuardExpr>>,
    pending_guards: Option<GuardExpr>,
    pending_inline_guards: Option<GuardExpr>,
    pending_can_open_block: bool,
    pending_scope_enters: usize,
    scope_stack: Vec<ScopeFrame>,
    pending_io_block: Option<PendingIoBlock>,
    io_scope_stack: Vec<IoScopeFrame>,
    block_stack: Vec<BlockKind>,
}

impl<'a> ScriptParser<'a> {
    pub fn new(input: &'a str) -> Result<Self> {
        let tokens = VecDeque::from(lexer::tokenize(input)?);
        Ok(Self {
            tokens,
            steps: Vec::new(),
            guard_stack: vec![None],
            pending_guards: None,
            pending_inline_guards: None,
            pending_can_open_block: false,
            pending_scope_enters: 0,
            scope_stack: Vec::new(),
            pending_io_block: None,
            io_scope_stack: Vec::new(),
            block_stack: Vec::new(),
        })
    }

    pub fn parse(mut self) -> Result<Vec<Step>> {
        while let Some(token) = self.tokens.pop_front() {
            if self.pending_io_block.is_some() && !matches!(token, RawToken::BlockStart { .. }) {
                let pending = self.pending_io_block.take().unwrap();
                bail!(
                    "line {}: WITH_IO block must be followed by '{{'",
                    pending.line_no
                );
            }
            match token {
                RawToken::Guard { pair, line_end } => {
                    let groups = parse_guard_line(pair)?;
                    self.handle_guard_token(line_end, groups)?
                }
                RawToken::BlockStart { line_no } => self.start_block(line_no)?,
                RawToken::BlockEnd { line_no } => self.end_block(line_no)?,
                RawToken::Command { pair, line_no } => {
                    let kind = parse_command(pair)?;
                    self.handle_command_token(line_no, kind)?
                }
            }
        }

        if let Some(pending) = self.pending_io_block.take() {
            bail!(
                "line {}: WITH_IO block must be followed by '{{'",
                pending.line_no
            );
        }

        if self.guard_stack.len() != 1 {
            bail!("unclosed guard block at end of script");
        }
        if self.pending_guards.is_some() {
            bail!("guard declared on final lines without a following command");
        }

        if let Some(frame) = self.io_scope_stack.last() {
            bail!(
                "WITH_IO block starting on line {} was not closed",
                frame.line_no
            );
        }

        // Validate `INHERIT_ENV` directives: only allowed in the prelude (before
        // any other commands) and at most one occurrence.
        {
            let mut seen_non_prelude = false;
            let mut inherit_count = 0usize;
            for step in &self.steps {
                match &step.kind {
                    StepKind::InheritEnv { .. } => {
                        if seen_non_prelude {
                            bail!("INHERIT_ENV must appear before any other commands");
                        }
                        if step.guard.is_some() || step.scope_enter > 0 || step.scope_exit > 0 {
                            bail!("INHERIT_ENV cannot be guarded or nested inside blocks");
                        }
                        inherit_count += 1;
                    }
                    kind => {
                        if contains_inherit_env(kind) {
                            bail!("INHERIT_ENV cannot be nested inside other commands");
                        }
                        seen_non_prelude = true;
                    }
                }
            }
            if inherit_count > 1 {
                bail!("only one INHERIT_ENV directive is allowed");
            }
        }

        Ok(self.steps)
    }

    fn handle_guard_token(&mut self, line_end: usize, expr: GuardExpr) -> Result<()> {
        if let Some(RawToken::Command { line_no, .. }) = self.tokens.front()
            && *line_no == line_end
        {
            self.pending_inline_guards = Some(expr);
            self.pending_can_open_block = false;
            return Ok(());
        }
        self.stash_pending_guard(expr);
        self.pending_can_open_block = true;
        Ok(())
    }

    fn handle_command_token(&mut self, line_no: usize, kind: StepKind) -> Result<()> {
        let inline = self.pending_inline_guards.take();
        self.handle_command(line_no, kind, inline)
    }

    fn stash_pending_guard(&mut self, guard: GuardExpr) {
        self.pending_guards = Some(if let Some(existing) = self.pending_guards.take() {
            GuardExpr::all(vec![existing, guard])
        } else {
            guard
        });
    }

    fn start_guard_block_from_pending(&mut self, line_no: usize) -> Result<()> {
        let guards = self
            .pending_guards
            .take()
            .ok_or_else(|| anyhow!("line {}: '{{' without a pending guard", line_no))?;
        if !self.pending_can_open_block {
            bail!("line {}: '{{' must directly follow a guard", line_no);
        }
        self.pending_can_open_block = false;
        self.enter_guard_block(guards, line_no)
    }

    fn enter_guard_block(&mut self, guard: GuardExpr, line_no: usize) -> Result<()> {
        let composed = if let Some(pending) = self.pending_guards.take() {
            GuardExpr::all(vec![pending, guard])
        } else {
            guard
        };
        let parent = self.guard_stack.last().cloned().unwrap_or(None);
        let next = and_guard_exprs(parent, Some(composed));
        self.guard_stack.push(next);
        self.scope_stack.push(ScopeFrame {
            line_no,
            had_command: false,
        });
        self.pending_scope_enters += 1;
        Ok(())
    }

    fn begin_io_block(
        &mut self,
        line_no: usize,
        bindings: Vec<IoBinding>,
        guards: Option<GuardExpr>,
    ) -> Result<()> {
        if self.pending_io_block.is_some() {
            bail!(
                "line {}: previous WITH_IO block is still waiting for '{{'",
                line_no
            );
        }
        self.pending_io_block = Some(PendingIoBlock {
            line_no,
            bindings,
            guards,
        });
        Ok(())
    }

    fn start_block(&mut self, line_no: usize) -> Result<()> {
        if let Some(pending) = self.pending_io_block.take() {
            self.block_stack.push(BlockKind::Io);
            self.io_scope_stack.push(IoScopeFrame {
                line_no: pending.line_no,
                had_command: false,
                bindings: pending.bindings,
                guards: pending.guards,
            });
            Ok(())
        } else {
            self.start_guard_block_from_pending(line_no)?;
            self.block_stack.push(BlockKind::Guard);
            Ok(())
        }
    }

    fn end_block(&mut self, line_no: usize) -> Result<()> {
        let kind = self
            .block_stack
            .pop()
            .ok_or_else(|| anyhow!("line {}: unexpected '}}'", line_no))?;
        match kind {
            BlockKind::Guard => self.end_guard_block(line_no),
            BlockKind::Io => self.end_io_block(line_no),
        }
    }

    fn end_guard_block(&mut self, line_no: usize) -> Result<()> {
        if self.guard_stack.len() == 1 {
            bail!("line {}: unexpected '}}'", line_no);
        }
        if self.pending_guards.is_some() {
            bail!(
                "line {}: guard declared immediately before '}}' without a command",
                line_no
            );
        }
        let frame = self
            .scope_stack
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("line {}: scope stack underflow", line_no))?;
        if !frame.had_command {
            bail!(
                "line {}: guard block starting on line {} must contain at least one command",
                line_no,
                frame.line_no
            );
        }
        let step = self
            .steps
            .last_mut()
            .ok_or_else(|| anyhow!("line {}: guard block closed without any commands", line_no))?;
        step.scope_exit += 1;
        self.scope_stack.pop();
        self.guard_stack.pop();
        Ok(())
    }

    fn end_io_block(&mut self, line_no: usize) -> Result<()> {
        let frame = self
            .io_scope_stack
            .pop()
            .ok_or_else(|| anyhow!("line {}: unexpected '}}'", line_no))?;
        if !frame.had_command {
            bail!(
                "line {}: WITH_IO block starting on line {} must contain at least one command",
                line_no,
                frame.line_no
            );
        }
        Ok(())
    }

    fn guard_context(&mut self, inline: Option<GuardExpr>) -> Option<GuardExpr> {
        let mut context = self.guard_stack.last().cloned().unwrap_or(None);
        if let Some(pending) = self.pending_guards.take() {
            context = and_guard_exprs(context, Some(pending));
            self.pending_can_open_block = false;
        }
        if let Some(inline_guard) = inline {
            context = and_guard_exprs(context, Some(inline_guard));
        }
        context
    }

    fn handle_command(
        &mut self,
        line_no: usize,
        kind: StepKind,
        inline_guards: Option<GuardExpr>,
    ) -> Result<()> {
        if let StepKind::WithIoBlock { bindings } = kind {
            let guards = self.guard_context(inline_guards);
            self.begin_io_block(line_no, bindings, guards)?;
            return Ok(());
        }

        let guards = self.guard_context(inline_guards);
        let guards = self.apply_io_guards(guards);
        let scope_enter = self.pending_scope_enters;
        self.pending_scope_enters = 0;
        for frame in self.scope_stack.iter_mut() {
            frame.had_command = true;
        }
        for frame in self.io_scope_stack.iter_mut() {
            frame.had_command = true;
        }
        let kind = self.apply_io_defaults(kind);
        self.steps.push(Step {
            guard: guards,
            kind,
            scope_enter,
            scope_exit: 0,
        });
        Ok(())
    }

    fn apply_io_defaults(&self, kind: StepKind) -> StepKind {
        let defaults = self.current_io_defaults();
        if defaults.is_empty() {
            return kind;
        }
        match kind {
            StepKind::WithIo { bindings, cmd } => StepKind::WithIo {
                bindings: merge_bindings(&defaults, &bindings),
                cmd,
            },
            other => StepKind::WithIo {
                bindings: defaults,
                cmd: Box::new(other),
            },
        }
    }

    fn current_io_defaults(&self) -> Vec<IoBinding> {
        if self.io_scope_stack.is_empty() {
            return Vec::new();
        }
        let mut set = IoBindingSet::default();
        for frame in &self.io_scope_stack {
            for binding in &frame.bindings {
                set.insert(binding.clone());
            }
        }
        set.into_vec()
    }

    fn apply_io_guards(&self, guard: Option<GuardExpr>) -> Option<GuardExpr> {
        self.io_scope_stack.iter().fold(guard, |acc, frame| {
            and_guard_exprs(acc, frame.guards.clone())
        })
    }
}

pub fn parse_script(input: &str) -> Result<Vec<Step>> {
    ScriptParser::new(input)?.parse()
}

fn and_guard_exprs(left: Option<GuardExpr>, right: Option<GuardExpr>) -> Option<GuardExpr> {
    match (left, right) {
        (None, None) => None,
        (Some(expr), None) | (None, Some(expr)) => Some(expr),
        (Some(lhs), Some(rhs)) => Some(GuardExpr::all(vec![lhs, rhs])),
    }
}

fn merge_bindings(defaults: &[IoBinding], overrides: &[IoBinding]) -> Vec<IoBinding> {
    let mut set = IoBindingSet::default();
    for binding in defaults {
        set.insert(binding.clone());
    }
    for binding in overrides {
        set.insert(binding.clone());
    }
    set.into_vec()
}

fn contains_inherit_env(kind: &StepKind) -> bool {
    match kind {
        StepKind::InheritEnv { .. } => true,
        StepKind::WithIo { cmd, .. } => contains_inherit_env(cmd),
        _ => false,
    }
}

fn parse_command(pair: Pair<Rule>) -> Result<StepKind> {
    let kind = match pair.as_rule() {
        Rule::workdir_command => {
            let arg = parse_single_arg(pair)?;
            StepKind::Workdir(arg.into())
        }
        Rule::workspace_command => {
            let target = parse_workspace_target(pair)?;
            StepKind::Workspace(target)
        }
        Rule::env_command => {
            let (key, value) = parse_env_pair(pair)?;
            StepKind::Env {
                key,
                value: value.into(),
            }
        }
        Rule::echo_command => {
            let msg = parse_message(pair)?;
            StepKind::Echo(msg.into())
        }
        Rule::run_command => {
            let cmd = parse_run_args(pair)?;
            StepKind::Run(cmd.into())
        }
        Rule::run_bg_command => {
            let cmd = parse_run_args(pair)?;
            StepKind::RunBg(cmd.into())
        }
        Rule::copy_command => {
            let mut args: Vec<String> = Vec::new();
            let mut from_current_workspace = false;
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::from_current_workspace_flag => from_current_workspace = true,
                    Rule::argument => args.push(parse_argument(inner)?),
                    _ => {}
                }
            }
            if args.len() != 2 {
                bail!("COPY expects 2 arguments (from, to)");
            }
            StepKind::Copy {
                from_current_workspace,
                from: args.remove(0).into(),
                to: args.remove(0).into(),
            }
        }
        Rule::with_io_command => {
            let mut bindings = Vec::new();
            let mut cmd = None;
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::io_flags => {
                        for flag in inner.into_inner() {
                            if flag.as_rule() == Rule::io_binding {
                                bindings.push(parse_io_binding(flag)?);
                            }
                        }
                    }
                    _ => {
                        cmd = Some(Box::new(parse_command(inner)?));
                    }
                }
            }
            if let Some(cmd) = cmd {
                StepKind::WithIo { bindings, cmd }
            } else {
                StepKind::WithIoBlock { bindings }
            }
        }
        Rule::copy_git_command => {
            let mut args = Vec::new();
            let mut include_dirty = false;
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::include_dirty_flag => include_dirty = true,
                    Rule::argument => args.push(parse_argument(inner)?),
                    _ => {}
                }
            }
            if args.len() != 3 {
                bail!("COPY_GIT expects 3 arguments (rev, from, to)");
            }
            StepKind::CopyGit {
                rev: args.remove(0).into(),
                from: args.remove(0).into(),
                to: args.remove(0).into(),
                include_dirty,
            }
        }
        Rule::hash_sha256_command => {
            let arg = parse_single_arg(pair)?;
            StepKind::HashSha256 { path: arg.into() }
        }
        Rule::inherit_env_command => {
            let mut keys: Vec<String> = Vec::new();
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::inherit_list => {
                        for key in inner.into_inner() {
                            if key.as_rule() == Rule::env_key {
                                keys.push(key.as_str().trim().to_string());
                            }
                        }
                    }
                    Rule::env_key => keys.push(inner.as_str().trim().to_string()),
                    _ => {}
                }
            }
            StepKind::InheritEnv { keys }
        }
        Rule::symlink_command => {
            let mut args = parse_args(pair)?;
            StepKind::Symlink {
                from: args.remove(0).into(),
                to: args.remove(0).into(),
            }
        }
        Rule::mkdir_command => {
            let arg = parse_single_arg(pair)?;
            StepKind::Mkdir(arg.into())
        }
        Rule::ls_command => {
            let args = parse_args(pair)?;
            StepKind::Ls(args.into_iter().next().map(Into::into))
        }
        Rule::cwd_command => StepKind::Cwd,
        Rule::read_command => {
            let args = parse_args(pair)?;
            StepKind::Read(args.into_iter().next().map(Into::into))
        }
        Rule::write_command => {
            let mut path = None;
            let mut contents = None;
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::argument if path.is_none() => {
                        path = Some(parse_argument(inner)?);
                    }
                    Rule::message => {
                        contents = Some(parse_concatenated_string(inner)?);
                    }
                    _ => {}
                }
            }
            StepKind::Write {
                path: path
                    .ok_or_else(|| anyhow!("WRITE expects a path argument"))?
                    .into(),
                contents: contents.map(Into::into),
            }
        }
        Rule::assert_file_hash_command => parse_assert_file_hash(pair)?,
        Rule::assert_file_content_command => parse_assert_file_content(pair)?,
        Rule::assert_dir_command => StepKind::AssertDir(parse_single_arg(pair)?.into()),
        Rule::assert_absent_command => StepKind::AssertAbsent(parse_single_arg(pair)?.into()),
        Rule::assert_stdout_command => StepKind::AssertStdout(parse_message(pair)?.into()),
        Rule::exit_command => {
            let code = parse_exit_code(pair)?;
            StepKind::Exit(code)
        }
        _ => bail!("unknown command rule: {:?}", pair.as_rule()),
    };
    Ok(kind)
}

fn parse_assert_file_hash(pair: Pair<Rule>) -> Result<StepKind> {
    let mut digest = None;
    let mut path = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::hash_digest => digest = Some(part.as_str().to_string()),
            Rule::argument => path = Some(parse_argument(part)?),
            _ => {}
        }
    }
    Ok(StepKind::AssertFile {
        hash: Some(digest.ok_or_else(|| anyhow!("missing hash digest"))?),
        path: path
            .ok_or_else(|| anyhow!("ASSERT_FILE --hash expects a path argument"))?
            .into(),
        contents: None,
    })
}

fn parse_assert_file_content(pair: Pair<Rule>) -> Result<StepKind> {
    let mut path = None;
    let mut contents = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::argument if path.is_none() => {
                path = Some(parse_argument(part)?);
            }
            Rule::message => {
                contents = Some(parse_concatenated_string(part)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::AssertFile {
        hash: None,
        path: path
            .ok_or_else(|| anyhow!("ASSERT_FILE expects a path argument"))?
            .into(),
        contents: contents.map(Into::into),
    })
}

fn parse_single_arg(pair: Pair<Rule>) -> Result<String> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::argument {
            return parse_argument(inner);
        }
    }
    bail!("missing argument")
}

fn parse_args(pair: Pair<Rule>) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::argument {
            args.push(parse_argument(inner)?);
        }
    }
    Ok(args)
}

fn parse_argument(pair: Pair<Rule>) -> Result<String> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => parse_quoted_string(inner),
        Rule::templated_arg => Ok(inner.as_str().to_string()),
        Rule::unquoted_arg => Ok(inner.as_str().to_string()),
        _ => unreachable!(),
    }
}

fn parse_quoted_string(pair: Pair<Rule>) -> Result<String> {
    let s = pair.as_str();
    let _quote = s.chars().next().unwrap();
    let content = &s[1..s.len() - 1];

    let mut out = String::with_capacity(content.len());
    let mut escape = false;
    for ch in content.chars() {
        if escape {
            out.push(ch);
            escape = false;
        } else if ch == '\\' {
            escape = true;
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn parse_workspace_target(pair: Pair<Rule>) -> Result<WorkspaceTarget> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::workspace_target {
            return match inner.as_str().to_ascii_lowercase().as_str() {
                "snapshot" => Ok(WorkspaceTarget::Snapshot),
                "local" => Ok(WorkspaceTarget::Local),
                _ => bail!("unknown workspace target"),
            };
        }
    }
    bail!("missing workspace target")
}

fn parse_env_pair(pair: Pair<Rule>) -> Result<(String, String)> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::env_pair {
            let mut parts = inner.into_inner();
            let key = parts.next().unwrap().as_str().to_string();
            let value_pair = parts.next().unwrap();
            let value = match value_pair.as_rule() {
                Rule::env_value_part => {
                    let inner_val = value_pair.into_inner().next().unwrap();
                    match inner_val.as_rule() {
                        Rule::quoted_string => parse_quoted_string(inner_val)?,
                        Rule::unquoted_env_value => inner_val.as_str().to_string(),
                        _ => unreachable!(
                            "unexpected rule in env_value_part: {:?}",
                            inner_val.as_rule()
                        ),
                    }
                }
                _ => unreachable!("expected env_value_part"),
            };
            return Ok((key, value));
        }
    }
    bail!("missing env pair")
}

fn parse_message(pair: Pair<Rule>) -> Result<String> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::message {
            return parse_concatenated_string(inner);
        }
    }
    bail!("missing message")
}

fn parse_run_args(pair: Pair<Rule>) -> Result<String> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::run_args {
            return parse_smart_concatenated_string(inner);
        }
    }
    bail!("missing run args")
}

fn parse_smart_concatenated_string(pair: Pair<Rule>) -> Result<String> {
    let parts: Vec<_> = pair.into_inner().collect();

    // Special case: If there is only one token and it is quoted, we assume the user
    // quoted it to satisfy the DSL (e.g. to include semicolons) but intends for the
    // content to be the raw command string. We unquote it unconditionally.
    if parts.len() == 1 && parts[0].as_rule() == Rule::quoted_string {
        return parse_quoted_string(parts[0].clone());
    }

    let mut body = String::new();
    let mut last_end = None;
    for part in parts {
        let span = part.as_span();
        if let Some(end) = last_end
            && span.start() > end
        {
            body.push(' ');
        }
        match part.as_rule() {
            Rule::quoted_string => {
                let raw = part.as_str();
                let unquoted = parse_quoted_string(part.clone())?;
                // Preserve quotes if the content needs them to be parsed correctly
                // by the shell (e.g. contains spaces, semicolons, etc).
                let needs_quotes = unquoted.is_empty()
                    || unquoted
                        .chars()
                        .any(|c| c.is_whitespace() || c == ';' || c == '\n' || c == '\r')
                    || unquoted.contains("//")
                    || unquoted.contains("/*");

                if needs_quotes {
                    body.push_str(raw);
                } else {
                    body.push_str(&unquoted);
                }
            }
            Rule::unquoted_msg_content | Rule::unquoted_run_content => body.push_str(part.as_str()),
            _ => {}
        }
        last_end = Some(span.end());
    }
    Ok(body)
}

fn parse_concatenated_string(pair: Pair<Rule>) -> Result<String> {
    let mut body = String::new();
    let mut last_end = None;
    for part in pair.into_inner() {
        let span = part.as_span();
        if let Some(end) = last_end
            && span.start() > end
        {
            body.push(' ');
        }
        match part.as_rule() {
            Rule::quoted_string => body.push_str(&parse_quoted_string(part)?),
            Rule::unquoted_msg_content | Rule::unquoted_run_content => body.push_str(part.as_str()),
            _ => {}
        }
        last_end = Some(span.end());
    }
    Ok(body)
}

fn parse_exit_code(pair: Pair<Rule>) -> Result<i32> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::exit_code {
            return inner
                .as_str()
                .parse()
                .map_err(|_| anyhow!("invalid exit code"));
        }
    }
    bail!("missing exit code")
}

fn parse_guard_line(pair: Pair<Rule>) -> Result<GuardExpr> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(inner);
        }
    }
    bail!("guard line missing expression")
}

fn parse_io_binding(pair: Pair<Rule>) -> Result<IoBinding> {
    let mut stream = None;
    let mut pipe = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::io_stream => stream = Some(parse_io_stream(inner.as_str())),
            Rule::pipe_binding => pipe = Some(parse_pipe_binding(inner)?),
            _ => {}
        }
    }
    let stream = stream.ok_or_else(|| anyhow!("missing IO stream in WITH_IO"))?;
    Ok(IoBinding { stream, pipe })
}

fn parse_io_stream(text: &str) -> IoStream {
    match text {
        "stdin" => IoStream::Stdin,
        "stdout" => IoStream::Stdout,
        "stderr" => IoStream::Stderr,
        _ => unreachable!("parser produced invalid io_stream token"),
    }
}

fn parse_pipe_binding(pair: Pair<Rule>) -> Result<String> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::pipe_name {
            return Ok(inner.as_str().to_string());
        }
    }
    bail!("missing pipe identifier in WITH_IO binding");
}

fn parse_guard_expr(pair: Pair<Rule>) -> Result<GuardExpr> {
    match pair.as_rule() {
        Rule::guard_expr => {
            let next = pair
                .into_inner()
                .next()
                .ok_or_else(|| anyhow!("guard expression missing body"))?;
            parse_guard_expr(next)
        }
        Rule::guard_seq => parse_guard_seq(pair),
        Rule::guard_factor => parse_guard_factor(pair),
        Rule::guard_not => parse_guard_not(pair),
        Rule::guard_primary => parse_guard_primary(pair),
        Rule::guard_group => parse_guard_group(pair),
        Rule::guard_or_call => parse_guard_or_call(pair),
        Rule::guard_term => Ok(GuardExpr::Predicate(parse_guard_term(pair)?)),
        _ => bail!("unexpected guard expression rule: {:?}", pair.as_rule()),
    }
}

fn parse_guard_seq(pair: Pair<Rule>) -> Result<GuardExpr> {
    let mut exprs = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_factor {
            exprs.push(parse_guard_factor(inner)?);
        }
    }
    match exprs.len() {
        0 => bail!("guard list requires at least one entry"),
        1 => Ok(exprs.pop().unwrap()),
        _ => Ok(GuardExpr::all(exprs)),
    }
}

fn parse_guard_factor(pair: Pair<Rule>) -> Result<GuardExpr> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_not {
            return parse_guard_not(inner);
        }
    }
    bail!("guard factor missing expression")
}

fn parse_guard_not(pair: Pair<Rule>) -> Result<GuardExpr> {
    let mut invert_count = 0usize;
    let mut primary = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::invert => invert_count += 1,
            _ => primary = Some(parse_guard_primary(inner)?),
        }
    }
    let expr = primary.ok_or_else(|| anyhow!("guard expression missing predicate"))?;
    apply_inversion(expr, invert_count % 2 == 1)
}

fn parse_guard_primary(pair: Pair<Rule>) -> Result<GuardExpr> {
    match pair.as_rule() {
        Rule::guard_primary => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| anyhow!("guard primary missing body"))?;
            parse_guard_primary(inner)
        }
        Rule::guard_group => parse_guard_group(pair),
        Rule::guard_or_call => parse_guard_or_call(pair),
        Rule::guard_term => Ok(GuardExpr::Predicate(parse_guard_term(pair)?)),
        _ => bail!("unexpected guard primary rule: {:?}", pair.as_rule()),
    }
}

fn parse_guard_group(pair: Pair<Rule>) -> Result<GuardExpr> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(inner);
        }
    }
    bail!("grouped guard missing expression")
}

fn parse_guard_or_call(pair: Pair<Rule>) -> Result<GuardExpr> {
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr_list {
            args = parse_guard_expr_list(inner)?;
        }
    }
    if args.len() < 2 {
        bail!("or(...) requires at least two guard expressions");
    }
    Ok(GuardExpr::or(args))
}

fn parse_guard_expr_list(pair: Pair<Rule>) -> Result<Vec<GuardExpr>> {
    let mut exprs = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            push_guard_or_args_from_expr(inner, &mut exprs)?;
        }
    }
    Ok(exprs)
}

fn push_guard_or_args_from_expr(expr_pair: Pair<Rule>, exprs: &mut Vec<GuardExpr>) -> Result<()> {
    if let Some(seq_pair) = expr_pair
        .clone()
        .into_inner()
        .find(|inner| inner.as_rule() == Rule::guard_seq)
    {
        let factors: Vec<Pair<Rule>> = seq_pair
            .into_inner()
            .filter(|inner| inner.as_rule() == Rule::guard_factor)
            .collect();
        if factors.len() > 1 {
            for factor in factors {
                exprs.push(parse_guard_factor(factor)?);
            }
            return Ok(());
        }
    }
    exprs.push(parse_guard_expr(expr_pair)?);
    Ok(())
}

fn apply_inversion(expr: GuardExpr, invert: bool) -> Result<GuardExpr> {
    if !invert {
        return Ok(expr);
    }
    match expr {
        GuardExpr::Predicate(guard) => {
            if let Guard::EnvEquals {
                key,
                value,
                invert: false,
            } = &guard
            {
                bail!(
                    "inverted env equality is not allowed: use 'env:{}!={}' or '!env:{}'",
                    key,
                    value,
                    key
                );
            }
            Ok(GuardExpr::Predicate(invert_guard(guard)))
        }
        other => Ok(!other),
    }
}

fn invert_guard(guard: Guard) -> Guard {
    match guard {
        Guard::Platform { target, invert } => Guard::Platform {
            target,
            invert: !invert,
        },
        Guard::EnvExists { key, invert } => Guard::EnvExists {
            key,
            invert: !invert,
        },
        Guard::EnvEquals { key, value, invert } => Guard::EnvEquals {
            key,
            value,
            invert: !invert,
        },
    }
}

fn parse_guard_term(pair: Pair<Rule>) -> Result<Guard> {
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::env_guard => return parse_env_guard(inner),
            Rule::bare_platform => return parse_bare_platform(inner, false),
            _ => {}
        }
    }
    bail!("missing guard predicate")
}

fn parse_env_guard(pair: Pair<Rule>) -> Result<Guard> {
    let mut key = String::new();
    let mut value = None;
    let mut is_not_equals = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::env_key => key = inner.as_str().trim().to_string(),
            Rule::env_comparison => {
                for comp_part in inner.into_inner() {
                    match comp_part.as_rule() {
                        Rule::equals_env | Rule::not_equals_env => {
                            for part in comp_part.into_inner() {
                                match part.as_rule() {
                                    Rule::eq_op => {}
                                    Rule::neq_op => is_not_equals = true,
                                    Rule::env_value => {
                                        value = Some(part.as_str().trim().to_string());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Rule::eq_op => {}
                        Rule::neq_op => is_not_equals = true,
                        Rule::env_value => {
                            value = Some(comp_part.as_str().trim().to_string());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(val) = value {
        Ok(Guard::EnvEquals {
            key,
            value: val,
            invert: is_not_equals,
        })
    } else {
        Ok(Guard::EnvExists { key, invert: false })
    }
}

fn parse_bare_platform(pair: Pair<Rule>, invert: bool) -> Result<Guard> {
    let tag = pair.into_inner().next().unwrap().as_str();
    parse_platform_tag(tag, invert)
}

fn parse_platform_tag(tag: &str, invert: bool) -> Result<Guard> {
    let target = match tag.to_ascii_lowercase().as_str() {
        "unix" => PlatformGuard::Unix,
        "windows" => PlatformGuard::Windows,
        "mac" | "macos" => PlatformGuard::Macos,
        "linux" => PlatformGuard::Linux,
        _ => bail!("unknown platform '{}'", tag),
    };
    Ok(Guard::Platform { target, invert })
}
