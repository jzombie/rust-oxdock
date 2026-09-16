use crate::ast::{
    Arg, Expr, Guard, GuardExpr, IoBinding, IoStream, PipeTarget, PlatformGuard, Step, StepKind,
    TypeKind,
};
use crate::command::ArgType;
use crate::error::{ParseError, ParseResult, SpanContext};
use crate::lexer::{self, RawToken, Rule, parse_pest_error, refine_span, span_for_line, span_of};
use pest::iterators::Pair;
use std::collections::VecDeque;
use std::str::FromStr;

#[derive(Clone)]
struct ScopeFrame {
    line_no: usize,
    had_command: bool,
}

#[derive(Clone)]
struct PendingIoBlock<'a> {
    line_no: usize,
    span: SpanContext<'a>,
    bindings: Vec<IoBinding>,
    guards: Option<GuardExpr>,
}

#[derive(Clone)]
struct IoScopeFrame {
    line_no: usize,
    had_command: bool,
    bindings: Vec<IoBinding>,
    guards: Option<GuardExpr>,
    /// Step index where this block's first command will land. Used to mark
    /// scope boundaries so WITH_IO block bodies scope LET/ENV/WORKDIR like
    /// every other braced block (only pipes leak).
    first_step: usize,
}

#[derive(Clone, Copy, Debug)]
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

pub struct ScriptParser<'a, F: Fn(&str, Vec<Arg>) -> ParseResult<StepKind>> {
    input: &'a str,
    tokens: VecDeque<RawToken<'a>>,
    steps: Vec<Step>,
    guard_stack: Vec<Option<GuardExpr>>,
    pending_guards: Option<GuardExpr>,
    pending_inline_guards: Option<GuardExpr>,
    pending_can_open_block: bool,
    pending_scope_enters: usize,
    scope_stack: Vec<ScopeFrame>,
    pending_io_block: Option<PendingIoBlock<'a>>,
    io_scope_stack: Vec<IoScopeFrame>,
    block_stack: Vec<BlockKind>,
    lower: F,
}

impl<'a, F: Fn(&str, Vec<Arg>) -> ParseResult<StepKind>> ScriptParser<'a, F> {
    pub fn new(input: &'a str, lower: F) -> ParseResult<Self> {
        let tokens = VecDeque::from(lexer::tokenize(input)?);
        Ok(Self {
            input,
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
            lower,
        })
    }

    /// Span for end of script errors (no failing token site).
    fn eof_span(&self) -> SpanContext<'_> {
        let lines = self.input.lines().count().max(1);
        span_for_line(self.input, lines)
    }

    pub fn parse(mut self) -> ParseResult<Vec<Step>> {
        while let Some(token) = self.tokens.pop_front() {
            let step_index = self.steps.len();
            if self.pending_io_block.is_some()
                && !matches!(
                    token,
                    RawToken::BlockStart { .. }
                        | RawToken::Command { .. }
                        | RawToken::Instruction { .. }
                        | RawToken::RunExec { .. }
                )
            {
                let pending = self.pending_io_block.take().unwrap();
                return Err(ParseError::structural(
                    "with_io",
                    format!(
                        "line {}: WITH_IO block must be followed by '{{'",
                        pending.line_no
                    ),
                    &pending.span,
                ));
            }
            match token {
                RawToken::Guard {
                    pair,
                    line_end,
                    span,
                } => {
                    let span = span.with_step(step_index);
                    let groups = parse_guard_line(&span, pair)?;
                    self.handle_guard_token(line_end, groups)?;
                }
                RawToken::BlockStart { line_no, span } => {
                    let span = span.with_step(step_index);
                    self.start_block(&span, line_no)?;
                }
                RawToken::BlockEnd { line_no, span } => {
                    let span = span.with_step(step_index);
                    self.end_block(&span, line_no)?;
                }
                RawToken::Command {
                    pair,
                    line_no,
                    span,
                } => {
                    let span = span.with_step(step_index);
                    let kind = parse_structural_command_with_lower(&span, pair, &self.lower)?;
                    self.handle_command_token(&span, line_no, kind)?;
                }
                RawToken::Instruction {
                    pair,
                    line_no,
                    span,
                } => {
                    let span = span.with_step(step_index);
                    let kind = self
                        .lower_instruction(&span, pair)
                        .map_err(|e| e.with_span(&span))?;
                    self.handle_command_token(&span, line_no, kind)?;
                }
                RawToken::RunExec {
                    pair,
                    line_no,
                    span,
                } => {
                    let span = span.with_step(step_index);
                    let kind = lower_run_exec_pair(&span, pair, &self.lower)?;
                    self.handle_command_token(&span, line_no, kind)?;
                }
            }
        }

        if let Some(pending) = self.pending_io_block.take() {
            return Err(ParseError::structural(
                "with_io",
                format!(
                    "line {}: WITH_IO block must be followed by '{{'",
                    pending.line_no
                ),
                &pending.span,
            ));
        }

        if self.guard_stack.len() != 1 {
            let ctx = self.eof_span();
            return Err(ParseError::structural(
                "guard",
                "unclosed guard block at end of script".to_string(),
                &ctx,
            ));
        }
        if self.pending_guards.is_some() {
            let ctx = self.eof_span();
            return Err(ParseError::structural(
                "guard",
                "guard declared on final lines without a following command".to_string(),
                &ctx,
            ));
        }

        if let Some(frame) = self.io_scope_stack.last() {
            let ctx = span_for_line(self.input, frame.line_no);
            return Err(ParseError::structural(
                "with_io",
                format!(
                    "WITH_IO block starting on line {} was not closed",
                    frame.line_no
                ),
                &ctx,
            ));
        }

        // Validate `INHERIT_ENV` directives: only allowed in the prelude (before
        // any other commands) and at most one occurrence.
        {
            let ctx = self.eof_span();
            let mut seen_non_prelude = false;
            let mut inherit_count = 0usize;
            for step in &self.steps {
                match &step.kind {
                    StepKind::InheritEnv { .. } => {
                        if seen_non_prelude {
                            return Err(ParseError::structural(
                                "inherit_env",
                                "INHERIT_ENV must appear before any other commands".to_string(),
                                &ctx,
                            ));
                        }
                        if step.guard.is_some() || step.scope_enter > 0 || step.scope_exit > 0 {
                            return Err(ParseError::structural(
                                "inherit_env",
                                "INHERIT_ENV cannot be guarded or nested inside blocks".to_string(),
                                &ctx,
                            ));
                        }
                        inherit_count += 1;
                    }
                    kind => {
                        if contains_inherit_env(kind) {
                            return Err(ParseError::structural(
                                "inherit_env",
                                "INHERIT_ENV cannot be nested inside other commands".to_string(),
                                &ctx,
                            ));
                        }
                        seen_non_prelude = true;
                    }
                }
            }
            if inherit_count > 1 {
                return Err(ParseError::structural(
                    "inherit_env",
                    "only one INHERIT_ENV directive is allowed".to_string(),
                    &ctx,
                ));
            }
        }

        Ok(self.steps)
    }

    fn lower_instruction(&self, ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
        lower_instruction_pair(ctx, pair, &self.lower)
    }

    fn handle_guard_token(&mut self, line_end: usize, expr: GuardExpr) -> ParseResult<()> {
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

    fn handle_command_token(
        &mut self,
        ctx: &SpanContext<'a>,
        line_no: usize,
        kind: StepKind,
    ) -> ParseResult<()> {
        let inline = self.pending_inline_guards.take();
        self.handle_command(ctx, line_no, kind, inline)
    }

    fn stash_pending_guard(&mut self, guard: GuardExpr) {
        self.pending_guards = Some(if let Some(existing) = self.pending_guards.take() {
            GuardExpr::all(vec![existing, guard])
        } else {
            guard
        });
    }

    fn start_guard_block_from_pending(
        &mut self,
        ctx: &SpanContext,
        line_no: usize,
    ) -> ParseResult<()> {
        let guards = self.pending_guards.take().ok_or_else(|| {
            ParseError::structural(
                "guard",
                format!("line {}: '{{' without a pending guard", line_no),
                ctx,
            )
        })?;
        if !self.pending_can_open_block {
            return Err(ParseError::structural(
                "guard",
                format!("line {}: '{{' must directly follow a guard", line_no),
                ctx,
            ));
        }
        self.pending_can_open_block = false;
        self.enter_guard_block(guards, line_no)
    }

    fn enter_guard_block(&mut self, guard: GuardExpr, line_no: usize) -> ParseResult<()> {
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
        ctx: &SpanContext<'a>,
        line_no: usize,
        bindings: Vec<IoBinding>,
        guards: Option<GuardExpr>,
    ) -> ParseResult<()> {
        if self.pending_io_block.is_some() {
            return Err(ParseError::structural(
                "with_io",
                format!(
                    "line {}: previous WITH_IO block is still waiting for '{{'",
                    line_no
                ),
                ctx,
            ));
        }
        self.pending_io_block = Some(PendingIoBlock {
            line_no,
            span: ctx.clone(),
            bindings,
            guards,
        });
        Ok(())
    }

    fn start_block(&mut self, ctx: &SpanContext, line_no: usize) -> ParseResult<()> {
        if let Some(pending) = self.pending_io_block.take() {
            self.block_stack.push(BlockKind::Io);
            self.io_scope_stack.push(IoScopeFrame {
                line_no: pending.line_no,
                had_command: false,
                bindings: pending.bindings,
                guards: pending.guards,
                first_step: self.steps.len(),
            });
            Ok(())
        } else {
            self.start_guard_block_from_pending(ctx, line_no)?;
            self.block_stack.push(BlockKind::Guard);
            Ok(())
        }
    }

    fn end_block(&mut self, ctx: &SpanContext, line_no: usize) -> ParseResult<()> {
        let kind = self.block_stack.pop().ok_or_else(|| {
            ParseError::structural("block", format!("line {}: unexpected '}}'", line_no), ctx)
        })?;
        match kind {
            BlockKind::Guard => self.end_guard_block(ctx, line_no),
            BlockKind::Io => self.end_io_block(ctx, line_no),
        }
    }

    fn end_guard_block(&mut self, ctx: &SpanContext, line_no: usize) -> ParseResult<()> {
        if self.guard_stack.len() == 1 {
            return Err(ParseError::structural(
                "guard",
                format!("line {}: unexpected '}}'", line_no),
                ctx,
            ));
        }
        if self.pending_guards.is_some() {
            return Err(ParseError::structural(
                "guard",
                format!(
                    "line {}: guard declared immediately before '}}' without a command",
                    line_no
                ),
                ctx,
            ));
        }
        let frame = self.scope_stack.last().cloned().ok_or_else(|| {
            ParseError::structural(
                "guard",
                format!("line {}: scope stack underflow", line_no),
                ctx,
            )
        })?;
        if !frame.had_command {
            return Err(ParseError::structural(
                "guard",
                format!(
                    "line {}: guard block starting on line {} must contain at least one command",
                    line_no, frame.line_no
                ),
                ctx,
            ));
        }
        let step = self.steps.last_mut().ok_or_else(|| {
            ParseError::structural(
                "guard",
                format!("line {}: guard block closed without any commands", line_no),
                ctx,
            )
        })?;
        step.scope_exit += 1;
        self.scope_stack.pop();
        self.guard_stack.pop();
        Ok(())
    }

    fn end_io_block(&mut self, ctx: &SpanContext, line_no: usize) -> ParseResult<()> {
        let frame = self.io_scope_stack.pop().ok_or_else(|| {
            ParseError::structural("with_io", format!("line {}: unexpected '}}'", line_no), ctx)
        })?;
        if !frame.had_command {
            return Err(ParseError::structural(
                "with_io",
                format!(
                    "line {}: WITH_IO block starting on line {} must contain at least one command",
                    line_no, frame.line_no
                ),
                ctx,
            ));
        }
        // WITH_IO block bodies are lexical scopes like guard blocks: mark
        // scope boundaries so LET/ENV/WORKDIR/WORKSPACE revert on exit.
        // Pipe registrations live in ExecIo and are unaffected (they leak).
        if self.steps.len() > frame.first_step {
            self.steps[frame.first_step].scope_enter += 1;
            if let Some(last) = self.steps.last_mut() {
                last.scope_exit += 1;
            }
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
        ctx: &SpanContext<'a>,
        line_no: usize,
        kind: StepKind,
        inline_guards: Option<GuardExpr>,
    ) -> ParseResult<()> {
        if let StepKind::WithIoBlock { bindings } = kind {
            let guards = self.guard_context(inline_guards);
            self.begin_io_block(ctx, line_no, bindings, guards)?;
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

pub fn parse_script(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<Vec<Step>> {
    ScriptParser::new(input, lower)?.parse()
}

pub fn parse_guard_expr_str(input: &str) -> ParseResult<GuardExpr> {
    use pest::Parser;
    let pairs = lexer::LanguageParser::parse(Rule::guard_expr, input).map_err(parse_pest_error)?;
    let pair = pairs.into_iter().next().ok_or_else(|| {
        ParseError::structural("guard", "empty guard".to_string(), &span_for_line(input, 1))
    })?;
    let ctx = span_of(&pair, input);
    parse_guard_expr(&ctx, pair)
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
        StepKind::AssignCapture { cmd, .. } => contains_inherit_env(cmd),
        StepKind::While { body, .. } | StepKind::FuncDef { body, .. } => {
            body.iter().any(|s| contains_inherit_env(&s.kind))
        }
        StepKind::Timeout { body, .. } | StepKind::AssignAsync { body, .. } => {
            body.iter().any(|s| contains_inherit_env(&s.kind))
        }
        _ => false,
    }
}

/// True when bindings reroute stdout into a named pipe. A `LET`-capture owns
/// the step's stdout, so combining the two is a parse error.
fn has_stdout_pipe(bindings: &[IoBinding]) -> bool {
    bindings
        .iter()
        .any(|b| b.stream == IoStream::Stdout && b.pipe.is_some())
}

/// Reject async machinery inside a capture body: background tasks are
/// captured via `LET $o: STRING = AWAIT $t`, never inline.
fn reject_async_in_capture(ctx: &SpanContext, kind: &StepKind) -> ParseResult<()> {
    let bad = match kind {
        StepKind::AsyncBlock { .. }
        | StepKind::AssignAsync { .. }
        | StepKind::Await { .. }
        | StepKind::AwaitCapture { .. }
        | StepKind::Cancel { .. } => true,
        StepKind::WithIo { cmd, .. } => reject_async_in_capture(ctx, cmd).is_err(),
        StepKind::Timeout { body, .. } => body
            .iter()
            .any(|s| reject_async_in_capture(ctx, &s.kind).is_err()),
        StepKind::While { body, .. } | StepKind::FuncDef { body, .. } => body
            .iter()
            .any(|s| reject_async_in_capture(ctx, &s.kind).is_err()),
        _ => false,
    };
    if bad {
        return Err(ParseError::structural("let", "LET capture cannot run ASYNC/AWAIT/CANCEL inline; use LET $t: HANDLE = ASYNC ... then LET $o: STRING = AWAIT $t".to_string(), ctx));
    }
    Ok(())
}

/// Reject `WITH_IO [stdout=pipe:...]` anywhere inside a capture body: the
/// capture sink owns stdout.
fn reject_pipe_stdout_in_capture(ctx: &SpanContext, kind: &StepKind) -> ParseResult<()> {
    match kind {
        StepKind::WithIo { bindings, cmd } => {
            if has_stdout_pipe(bindings) {
                return Err(ParseError::structural("let", "LET capture cannot use WITH_IO [stdout=pipe:...]; the capture sink owns stdout".to_string(), ctx));
            }
            reject_pipe_stdout_in_capture(ctx, cmd)
        }
        StepKind::Timeout { body, .. } => {
            for step in body {
                reject_pipe_stdout_in_capture(ctx, &step.kind)?;
            }
            Ok(())
        }
        StepKind::While { body, .. } | StepKind::FuncDef { body, .. } => {
            for step in body {
                reject_pipe_stdout_in_capture(ctx, &step.kind)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Re-parse raw RHS text as an expression (fallback when the `LET` RHS lead
/// token is not a known command). Requires the expression to consume the
/// full text so `LET $x: STRING = FOO bar` stays an error instead of binding `FOO`.
fn parse_expr_str(ctx: &SpanContext, text: &str) -> ParseResult<Expr> {
    use pest::Parser;
    let mut pairs = lexer::LanguageParser::parse(Rule::expr, text).map_err(parse_pest_error)?;
    let pair = pairs.next().ok_or_else(|| {
        ParseError::validation("LET", "LET requires an expression".to_string(), ctx)
    })?;
    if pair.as_span().end() != text.len() {
        return Err(ParseError::structural(
            "expr",
            format!("invalid LET expression {text:?}"),
            ctx,
        ));
    }
    parse_expr(ctx, pair)
}

fn parse_structural_command_with_lower(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let kind = match pair.as_rule() {
        Rule::inherit_env_command => {
            let mut keys = Vec::new();
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::inherit_list {
                    for key in inner.into_inner() {
                        if key.as_rule() == Rule::env_key {
                            keys.push(key.as_str().trim().to_string());
                        }
                    }
                } else if inner.as_rule() == Rule::env_key {
                    keys.push(inner.as_str().trim().to_string());
                }
            }
            StepKind::InheritEnv { keys }
        }
        Rule::with_io_command => {
            let mut bindings = Vec::new();
            let mut cmd = None;
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::io_flags => {
                        for flag in inner.into_inner() {
                            if flag.as_rule() == Rule::io_binding {
                                bindings.push(parse_io_binding(ctx, flag)?);
                            }
                        }
                    }
                    Rule::with_io_command => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(
                            ctx, inner, lower,
                        )?));
                    }
                    Rule::inherit_env_command => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(
                            ctx, inner, lower,
                        )?));
                    }
                    Rule::async_statement | Rule::async_statement_block => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(
                            ctx, inner, lower,
                        )?));
                    }
                    Rule::timeout_statement | Rule::cancel_statement => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(
                            ctx, inner, lower,
                        )?));
                    }
                    Rule::call_statement | Rule::while_statement => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(
                            ctx, inner, lower,
                        )?));
                    }
                    Rule::func_def
                    | Rule::return_statement
                    | Rule::break_statement
                    | Rule::continue_statement => {
                        return Err(ParseError::structural(
                            "parser",
                            format!(
                                "WITH_IO cannot wrap {:?}; place it around a command or block instead",
                                inner.as_rule()
                            ),
                            &span,
                        ));
                    }
                    Rule::instruction | Rule::instruction_inner => {
                        cmd = Some(Box::new(lower_instruction_pair(ctx, inner, lower)?));
                    }
                    Rule::run_exec_statement | Rule::run_exec_inner => {
                        cmd = Some(Box::new(lower_run_exec_pair(ctx, inner, lower)?));
                    }
                    _ => {}
                }
            }
            if let Some(cmd) = cmd {
                StepKind::WithIo { bindings, cmd }
            } else {
                StepKind::WithIoBlock { bindings }
            }
        }
        Rule::for_statement => parse_for_statement_from_pair(ctx, pair, lower)?,
        Rule::while_statement => parse_while_statement_from_pair(ctx, pair, lower)?,
        Rule::func_def => parse_func_def_from_pair(ctx, pair, lower)?,
        Rule::call_statement => parse_call_statement_from_pair(ctx, pair)?,
        Rule::return_statement => parse_return_statement_from_pair(ctx, pair)?,
        Rule::break_statement => StepKind::Break,
        Rule::continue_statement => StepKind::Continue,
        Rule::let_statement => parse_let_statement_from_pair(ctx, pair)?,
        Rule::mutate_statement => parse_mutate_statement_from_pair(ctx, pair)?,
        Rule::let_async_statement => parse_let_async_statement_from_pair(ctx, pair, lower)?,
        Rule::let_capture_statement => parse_let_capture_statement_from_pair(ctx, pair, lower)?,
        Rule::await_statement => parse_await_statement_from_pair(ctx, pair)?,
        Rule::cancel_statement => parse_cancel_statement_from_pair(ctx, pair)?,
        Rule::if_statement => parse_if_statement_from_pair(ctx, pair, lower)?,
        Rule::async_statement => parse_async_statement_from_pair(ctx, pair, lower)?,
        Rule::async_statement_block => parse_async_statement_block_from_pair(ctx, pair, lower)?,
        Rule::timeout_statement => parse_timeout_statement_from_pair(ctx, pair, lower)?,
        Rule::command_inner => {
            // command_inner = { inherit_env_command | instruction }
            // Unwrap to the inner rule
            let inner = pair.into_inner().next().ok_or_else(|| {
                ParseError::structural("parser", "empty command_inner".to_string(), &span)
            })?;
            parse_structural_command_with_lower(ctx, inner, lower)?
        }
        Rule::instruction | Rule::instruction_inner => lower_instruction_pair(ctx, pair, lower)?,
        Rule::run_exec_statement | Rule::run_exec_inner => lower_run_exec_pair(ctx, pair, lower)?,
        _ => {
            return Err(ParseError::structural(
                "parser",
                format!("unexpected structural command rule: {:?}", pair.as_rule()),
                &span,
            ));
        }
    };
    Ok(kind)
}

fn extract_instruction(
    ctx: &SpanContext,
    pair: Pair<Rule>,
) -> ParseResult<(String, Vec<InsToken>)> {
    let span = refine_span(ctx, &pair);
    let mut name = None;
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::command_name => {
                name = Some(inner.as_str().to_string());
            }
            Rule::argument => {
                args.extend(parse_argument(ctx, inner)?.into_iter().map(InsToken::Pos));
            }
            Rule::assignment => {
                let (key, value) = parse_assignment(ctx, inner)?;
                args.push(InsToken::Assign(key, value));
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| {
        ParseError::structural(
            "instruction",
            "instruction missing command name".to_string(),
            &span,
        )
    })?;
    Ok((name, args))
}

/// One lowered instruction token: a positional argument, or a pre-split
/// `KEY=value` assignment from the unified grammar rule. Assignments reach
/// ENV/EXPAND lowerings intact; every other command sees them collapsed to
/// canonical `key=value` text (see `lower_instruction_pair`).
enum InsToken {
    Pos(Arg),
    Assign(String, Arg),
}

/// Lower one generic instruction pair: ENV/EXPAND build `StepKind` directly
/// from pre-split assignments (never via the injected `lower`, mirroring how
/// LET/FOR/IF bypass it); all other commands flow through `lower` with
/// assignments in canonical text form.
fn lower_instruction_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let (name, tokens) = extract_instruction(ctx, pair)?;
    if name == "ENV" {
        return lower_env_command(ctx, tokens);
    }
    if name == "EXPAND" {
        return lower_expand_command(ctx, tokens);
    }
    let args = tokens
        .into_iter()
        .map(|token| match token {
            InsToken::Pos(arg) => arg,
            InsToken::Assign(key, value) => crate::commands::canonical_assignment_arg(&key, &value),
        })
        .collect();
    lower(&name, args).map_err(|e| e.with_span(&span))
}

/// Lower a `run_exec` grammar pair: the PEG engine has already validated the
/// full `RUN [...]` span, so extract the inner `list_literal` and route the
/// structured `Expr::List` through the injected `lower` as `RUN` with one
/// typed argument (production `lower_command` maps it to `StepKind::RunExec`;
/// the grammar-test mock wraps it in `StepKind::Run`).
fn lower_run_exec_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut list = None;
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::run_exec_list {
            list = Some(parse_run_exec_list(ctx, inner)?);
        }
    }
    let list = list.ok_or_else(|| {
        ParseError::structural(
            "run_exec",
            "RUN exec form missing list literal".to_string(),
            &span,
        )
    })?;
    lower("RUN", vec![Arg::Expr(list)]).map_err(|e| e.with_span(&span))
}

/// Lower a `run_exec_list` pair: like `parse_list_literal` but elements are
/// atoms only (see `run_exec_arg` in the grammar), so shell bracket content
/// never parses here. Numeric atoms lower exactly like expression atoms
/// (including the `i64::MIN` boundary rejection).
fn parse_run_exec_list(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let mut items = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::run_exec_arg {
            let item = parse_run_exec_arg(ctx, inner)?;
            reject_boundary(ctx, &item)?;
            items.push(item);
        }
    }
    Ok(Expr::List(items))
}

fn parse_run_exec_arg(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let inner = pair.into_inner().next().ok_or_else(|| {
        ParseError::structural("run_exec", "RUN exec argument is empty".to_string(), &span)
    })?;
    match inner.as_rule() {
        Rule::parenthesized_expr => parse_expr_inner(ctx, inner.into_inner().next().unwrap()),
        Rule::func_call => parse_func_call(ctx, inner),
        Rule::key_path => parse_key_path(ctx, inner),
        Rule::variable => {
            let name = inner.as_str();
            let name = name.strip_prefix('$').unwrap_or(name).to_string();
            Ok(Expr::Var(name))
        }
        Rule::env_read => parse_env_read(ctx, inner).map(Expr::Env),
        Rule::pipe_read => parse_pipe_read(ctx, inner).map(|name| Expr::Literal(Value::Pipe(name))),
        Rule::list_literal => parse_list_literal(ctx, inner),
        Rule::map_literal => parse_map_literal(ctx, inner),
        Rule::string_literal | Rule::quoted_string => {
            let s = parse_quoted_string(inner)?;
            Ok(Expr::Literal(Value::String(s)))
        }
        Rule::numeric_literal => parse_numeric_literal(ctx, inner),
        Rule::bare_word => {
            let s = inner.as_str().to_string();
            match s.as_str() {
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                _ => Ok(Expr::Literal(Value::String(s))),
            }
        }
        _ => Err(ParseError::structural(
            "run_exec",
            format!("unexpected RUN exec argument rule: {:?}", inner.as_rule()),
            &span,
        )),
    }
}

/// Split one `assignment` pair into its key and lowered value.
fn parse_assignment(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<(String, Arg)> {
    let span = refine_span(ctx, &pair);
    let mut key = None;
    let mut value = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::assign_key => {
                key = Some(inner.as_str().to_string());
            }
            Rule::assign_value => {
                value = Some(lower_command_value(ctx, inner)?);
            }
            _ => {
                return Err(ParseError::structural(
                    "assignment",
                    format!("unexpected assignment rule: {:?}", inner.as_rule()),
                    &span,
                ));
            }
        }
    }
    Ok((
        key.ok_or_else(|| {
            ParseError::structural("assignment", "assignment missing key".to_string(), &span)
        })?,
        value.unwrap_or(Arg::String(String::new(), false)),
    ))
}

/// Single unified value lowering: every command's free-text value flows through
/// here on raw pest spans. Quoted bytes stay exact, lone `$var`/`$a.b`/`CALL()`
/// stay typed `Arg::Expr`, and anything else becomes literal text with only
/// `{{ }}` as the interpolation trigger. No heuristic rewriting, ever.
fn lower_command_value(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Arg> {
    let span = refine_span(ctx, &pair);
    let inner = pair.into_inner().next().ok_or_else(|| {
        ParseError::structural("assignment", "assignment value is empty".to_string(), &span)
    })?;
    match inner.as_rule() {
        Rule::quoted_string => Ok(Arg::String(parse_quoted_string(inner)?, true)),
        Rule::assign_expr => {
            let shape = inner.into_inner().next().ok_or_else(|| {
                ParseError::structural(
                    "assignment",
                    "assignment expression is empty".to_string(),
                    &span,
                )
            })?;
            match shape.as_rule() {
                Rule::variable => Ok(Arg::Expr(Expr::Var(parse_dollar_ident(shape)))),
                Rule::key_path => Ok(Arg::Expr(parse_key_path(ctx, shape)?)),
                Rule::env_read => Ok(Arg::Expr(Expr::Env(parse_env_read(ctx, shape)?))),
                Rule::func_call => Ok(Arg::Expr(parse_func_call(ctx, shape)?)),
                other => Err(ParseError::structural(
                    "assignment",
                    format!("unexpected assignment expression shape: {:?}", other),
                    &span,
                )),
            }
        }
        Rule::raw_fragments => lower_raw_fragments(ctx, inner),
        other => Err(ParseError::structural(
            "assignment",
            format!("unexpected assignment value rule: {:?}", other),
            &span,
        )),
    }
}

/// Assemble a bounded raw span into one literal `Arg::String`: `{{ }}` template
/// chunks pass through verbatim for `expand_string`, quoted chunks unquote
/// once with exact bytes, and unquoted runs collapse whitespace to single
/// spaces (trailing/leading edges trimmed). Pure text needs no `Parts` — every
/// fragment resolves through the same `expand_string` pass.
fn lower_raw_fragments(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Arg> {
    let span = refine_span(ctx, &pair);
    let mut body = String::new();
    for fragment in pair.into_inner() {
        match fragment.as_rule() {
            Rule::quoted_string => body.push_str(&parse_quoted_string(fragment)?),
            Rule::templated_arg => body.push_str(fragment.as_str()),
            Rule::raw_text => body.push_str(&collapse_ws(fragment.as_str())),
            other => {
                return Err(ParseError::structural(
                    "assignment",
                    format!("unexpected raw value fragment: {:?}", other),
                    &span,
                ));
            }
        }
    }
    Ok(Arg::String(body.trim().to_string(), false))
}

/// Collapse every whitespace run to a single space, preserving edge positions
/// (callers trim the assembled value).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// Parser-direct `ENV` lowering: exactly one assignment. A lone positional
/// holding `=` is the exotic-key fringe (keys the grammar cannot classify);
/// anything else is a precise error instead of a silent drop.
fn lower_env_command(ctx: &SpanContext, tokens: Vec<InsToken>) -> ParseResult<StepKind> {
    if tokens.is_empty() {
        return Err(ParseError::validation(
            "ENV",
            "ENV requires KEY=value".to_string(),
            ctx,
        ));
    }
    match tokens.as_slice() {
        [InsToken::Assign(key, value)] => {
            // Same KeyValue check the central validator applies on the
            // `lower_command` path, over the joined assignment form.
            ArgType::KeyValue
                .check_arg(&Arg::String(format!("{key}={}", value.render()), false))
                .map_err(|e| ParseError::validation("ENV", e.to_string(), ctx))?;
            Ok(StepKind::Env {
                key: key.clone(),
                value: value.clone(),
            })
        }
        [InsToken::Pos(Arg::String(text, _))] => match crate::command::split_assignment(text)
            .map_err(|e| ParseError::validation("ENV", e.to_string(), ctx))?
        {
            Some((key, value)) => Ok(StepKind::Env { key, value }),
            None => Err(ParseError::validation(
                "ENV",
                "ENV requires KEY=value format".to_string(),
                ctx,
            )),
        },
        _ => Err(ParseError::validation(
            "ENV",
            "ENV requires KEY=value format".to_string(),
            ctx,
        )),
    }
}

/// Parser-direct `EXPAND` lowering: positional tokens are the optional path,
/// assignments are overrides. Split quoted values can never masquerade as
/// extra paths — tokenize time already proved they are one value.
fn lower_expand_command(ctx: &SpanContext, tokens: Vec<InsToken>) -> ParseResult<StepKind> {
    let mut path = None;
    let mut overrides = Vec::new();
    for token in tokens {
        match token {
            InsToken::Assign(key, value) => {
                if key.is_empty() {
                    return Err(ParseError::validation(
                        "EXPAND",
                        "EXPAND requires KEY=value format for overrides".to_string(),
                        ctx,
                    ));
                }
                overrides.push((key, value));
            }
            InsToken::Pos(arg) => match &arg {
                Arg::String(text, quoted) if !quoted && text.contains('=') => {
                    let Some((key, value)) = crate::command::split_assignment(text)
                        .map_err(|e| ParseError::validation("EXPAND", e.to_string(), ctx))?
                    else {
                        return Err(ParseError::validation(
                            "EXPAND",
                            "EXPAND requires KEY=value format for overrides".to_string(),
                            ctx,
                        ));
                    };
                    overrides.push((key, value));
                }
                _ => {
                    if path.is_none() {
                        // Path-typed positional, checked like every other
                        // `lower_command` path arg (literals always pass;
                        // resolution stays runtime).
                        ArgType::Path
                            .check_arg(&arg)
                            .map_err(|e| ParseError::validation("EXPAND", e.to_string(), ctx))?;
                        path = Some(arg);
                    } else {
                        return Err(ParseError::validation(
                            "EXPAND",
                            "EXPAND accepts at most one path".to_string(),
                            ctx,
                        ));
                    }
                }
            },
        }
    }
    Ok(StepKind::Expand { path, overrides })
}

fn parse_type_tag(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<TypeKind> {
    let span = refine_span(ctx, &pair);
    TypeKind::from_str(pair.as_str().trim())
        .map_err(|e| ParseError::structural("type", e.to_string(), &span))
}

fn check_func_ident(ctx: &SpanContext, name: &str) -> ParseResult<()> {
    let ok = name
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(ParseError::validation(
            "FUNC",
            format!(
                "function names must be UPPERCASE (ASCII_ALPHA_UPPER, digits, _), got `{name}`"
            ),
            ctx,
        ));
    }
    Ok(())
}

fn parse_while_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut cond = None;
    let mut body = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => {
                if cond.is_none() {
                    cond = Some(parse_expr(ctx, inner)?);
                }
            }
            Rule::block => {
                body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::While {
        cond: Box::new(cond.ok_or_else(|| {
            ParseError::validation("WHILE", "WHILE requires a condition".to_string(), &span)
        })?),
        body: body.ok_or_else(|| {
            ParseError::validation("WHILE", "WHILE requires a block".to_string(), &span)
        })?,
    })
}

fn parse_func_def_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut name: Option<String> = None;
    let mut param_names: Vec<String> = Vec::new();
    let mut param_types: Vec<TypeKind> = Vec::new();
    let mut body = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::func_ident => {
                if name.is_none() {
                    name = Some(inner.as_str().to_string());
                }
            }
            Rule::func_param => {
                let mut pname = None;
                let mut ptype = None;
                for part in inner.into_inner() {
                    match part.as_rule() {
                        Rule::dollar_ident => {
                            pname = Some(parse_dollar_ident(part));
                        }
                        Rule::type_tag => {
                            ptype = Some(parse_type_tag(ctx, part)?);
                        }
                        _ => {}
                    }
                }
                param_names.push(pname.ok_or_else(|| {
                    ParseError::validation(
                        "FUNC",
                        "FUNC parameter requires a $variable".to_string(),
                        &span,
                    )
                })?);
                param_types.push(ptype.ok_or_else(|| {
                    ParseError::validation(
                        "FUNC",
                        "FUNC parameters require explicit types: FUNC NAME($p: TYPE, ...)"
                            .to_string(),
                        &span,
                    )
                })?);
            }
            Rule::block => {
                body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
            }
            _ => {}
        }
    }
    let name = name
        .ok_or_else(|| ParseError::validation("FUNC", "FUNC requires a name".to_string(), &span))?;
    check_func_ident(ctx, &name)?;
    if param_names.len() != param_types.len() {
        return Err(ParseError::validation(
            "FUNC",
            format!("FUNC {name} has mismatched parameter names and types"),
            &span,
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for pname in &param_names {
        if !seen.insert(pname.clone()) {
            return Err(ParseError::validation(
                "FUNC",
                format!("FUNC {name} declares duplicate parameter ${pname}"),
                &span,
            ));
        }
    }
    Ok(StepKind::FuncDef {
        name,
        params: param_names.into_iter().zip(param_types).collect(),
        body: body.ok_or_else(|| {
            ParseError::validation("FUNC", "FUNC requires a block".to_string(), &span)
        })?,
    })
}

fn parse_call_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut name: Option<String> = None;
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::func_ident => {
                if name.is_none() {
                    name = Some(inner.as_str().to_string());
                }
            }
            Rule::expr => {
                args.push(parse_expr(ctx, inner)?);
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| {
        ParseError::validation("CALL", "CALL requires a function name".to_string(), &span)
    })?;
    check_func_ident(ctx, &name)?;
    Ok(StepKind::Call { name, args })
}

fn parse_return_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    use crate::ast::Value;
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::expr {
            return Ok(StepKind::Return {
                expr: Box::new(parse_expr(ctx, inner)?),
            });
        }
    }
    Ok(StepKind::Return {
        expr: Box::new(Expr::Literal(Value::String(String::new()))),
    })
}

fn parse_for_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut idents: Vec<String> = Vec::new();
    let mut types: Vec<TypeKind> = Vec::new();
    let mut type_spans: Vec<SpanContext> = Vec::new();
    let mut in_expr = None;
    let mut body_steps = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                idents.push(parse_dollar_ident(inner));
            }
            Rule::type_tag => {
                type_spans.push(refine_span(ctx, &inner));
                types.push(parse_type_tag(ctx, inner)?);
            }
            Rule::expr => {
                in_expr = Some(parse_expr(ctx, inner)?);
            }
            Rule::block => {
                body_steps = parse_block_elements_with_lower(ctx, inner, lower)?;
            }
            _ => {}
        }
    }
    if idents.len() != types.len() {
        return Err(ParseError::validation(
            "FOR",
            format!(
                "FOR requires explicit types: FOR $item: TYPE IN <expr> (got {} vars, {} types)",
                idents.len(),
                types.len()
            ),
            &span,
        ));
    }
    let (key_var, key_type, var, var_type) = match idents.len() {
        1 => (
            None,
            None,
            idents.into_iter().next().unwrap(),
            types.into_iter().next().unwrap(),
        ),
        2 => {
            let mut iv = idents.into_iter();
            let mut tv = types.into_iter();
            (
                Some(iv.next().unwrap()),
                Some(tv.next().unwrap()),
                iv.next().unwrap(),
                tv.next().unwrap(),
            )
        }
        _ => {
            return Err(ParseError::validation(
                "FOR",
                "FOR requires one or two variables".to_string(),
                &span,
            ));
        }
    };
    if let Some(kt) = &key_type
        && *kt != TypeKind::String
        && *kt != TypeKind::Int
    {
        // Pinpoint the offending key type tag rather than the statement.
        let at = type_spans.first().unwrap_or(&span);
        return Err(ParseError::validation(
            "FOR",
            format!("FOR key variable must be INT or STRING, got {kt}"),
            at,
        ));
    }
    Ok(StepKind::For {
        key_var,
        key_type,
        var,
        var_type,
        in_expr: in_expr.ok_or_else(|| {
            ParseError::validation(
                "FOR",
                "FOR requires an iterable expression".to_string(),
                &span,
            )
        })?,
        body: body_steps,
    })
}

fn parse_let_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut var = None;
    let mut decl_type = None;
    let mut expr = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                var = Some(parse_dollar_ident(inner));
            }
            Rule::type_tag => {
                decl_type = Some(parse_type_tag(ctx, inner)?);
            }
            Rule::expr => {
                expr = Some(parse_expr(ctx, inner)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::Assign {
        var: var.ok_or_else(|| {
            ParseError::validation("LET", "LET requires a variable".to_string(), &span)
        })?,
        decl_type: decl_type.ok_or_else(|| {
            ParseError::validation(
                "LET",
                "LET requires explicit type: LET $var: TYPE = <expr>".to_string(),
                &span,
            )
        })?,
        expr: expr.ok_or_else(|| {
            ParseError::validation("LET", "LET requires an expression".to_string(), &span)
        })?,
    })
}

fn parse_mutate_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut var = None;
    let mut expr = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                var = Some(parse_dollar_ident(inner));
            }
            Rule::expr => {
                expr = Some(parse_expr(ctx, inner)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::Set {
        var: var.ok_or_else(|| {
            ParseError::validation(
                "mutate",
                "mutation requires a variable: $var = <expr>".to_string(),
                &span,
            )
        })?,
        expr: expr.ok_or_else(|| {
            ParseError::validation(
                "mutate",
                "mutation requires an expression: $var = <expr>".to_string(),
                &span,
            )
        })?,
    })
}

fn parse_let_async_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut var = None;
    let mut decl_type: Option<TypeKind> = None;
    let mut body = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                var = Some(parse_dollar_ident(inner));
            }
            Rule::type_tag => {
                decl_type = Some(parse_type_tag(ctx, inner)?);
            }
            Rule::block => {
                body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
            }
            Rule::command_inner => {
                // command_inner = { inherit_env_command | async_statement | async_statement_block | instruction }
                // Unwrap to the inner rule
                let inner = inner.into_inner().next().ok_or_else(|| {
                    ParseError::structural("let", "empty command_inner".to_string(), &span)
                })?;
                let step_kind = parse_structural_command_with_lower(ctx, inner, lower)?;
                body = Some(vec![Step {
                    guard: None,
                    kind: step_kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            Rule::with_io_command => {
                // LET $var: TYPE = WITH_IO [flags] ... — two shapes share this rule
                // (`let_async_statement` precedes `let_capture_statement` in
                // the grammar, so every WITH_IO-led LET lands here):
                // - wrapping ASYNC binds a pipe-wired background task. The
                //   bindings apply inside the task thread — the same shape as
                //   a braced body holding one WITH_IO step, which the
                //   AssignAsync runtime path supports.
                // - wrapping a synchronous command captures its stdout into
                //   the variable (same semantics as LET $x: STRING = <command>).
                let kind = parse_structural_command_with_lower(ctx, inner, lower)?;
                let StepKind::WithIo { bindings, cmd } = kind else {
                    return Err(ParseError::validation("LET", "LET $var: TYPE = WITH_IO requires an ASYNC command (e.g. LET $t = WITH_IO [stdin=pipe:p] ASYNC WRITE \"f\")".to_string(), &span));
                };
                match *cmd {
                    StepKind::AsyncBlock { body: async_body } => {
                        if async_body.len() != 1 {
                            return Err(ParseError::structural("let", "LET $var: TYPE = WITH_IO [..] ASYNC accepts a single command; use LET $var: HANDLE = ASYNC {{ ... }} with WITH_IO inside the block for multi-step tasks".to_string(), &span));
                        }
                        let step = async_body.into_iter().next().ok_or_else(|| {
                            ParseError::validation(
                                "LET",
                                "LET $var: HANDLE = ASYNC requires a body".to_string(),
                                &span,
                            )
                        })?;
                        body = Some(vec![Step {
                            guard: step.guard,
                            kind: StepKind::WithIo {
                                bindings,
                                cmd: Box::new(step.kind),
                            },
                            scope_enter: step.scope_enter,
                            scope_exit: step.scope_exit,
                        }]);
                    }
                    sync_cmd => {
                        if has_stdout_pipe(&bindings) {
                            return Err(ParseError::structural("let", "LET capture cannot use WITH_IO [stdout=pipe:...]; the capture sink owns stdout".to_string(), &span));
                        }
                        reject_async_in_capture(ctx, &sync_cmd)?;
                        let name = var.clone().ok_or_else(|| {
                            ParseError::validation(
                                "LET",
                                "LET $var: TYPE = WITH_IO requires a variable".to_string(),
                                &span,
                            )
                        })?;
                        let dtype = decl_type.ok_or_else(|| {
                            ParseError::validation(
                                "LET",
                                "LET requires explicit type: LET $var: TYPE = ...".to_string(),
                                &span,
                            )
                        })?;
                        return Ok(StepKind::AssignCapture {
                            var: name,
                            decl_type: dtype,
                            cmd: Box::new(StepKind::WithIo {
                                bindings,
                                cmd: Box::new(sync_cmd),
                            }),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(StepKind::AssignAsync {
        var: var.ok_or_else(|| {
            ParseError::validation(
                "LET",
                "LET $var: HANDLE = ASYNC requires a variable".to_string(),
                &span,
            )
        })?,
        decl_type: decl_type.ok_or_else(|| {
            ParseError::validation(
                "LET",
                "LET requires explicit type: LET $var: TYPE = ...".to_string(),
                &span,
            )
        })?,
        body: body.ok_or_else(|| {
            ParseError::validation(
                "LET",
                "LET $var: HANDLE = ASYNC requires a body".to_string(),
                &span,
            )
        })?,
    })
}

/// Lower `LET $var: STRING = <sync command>` / `LET $out: STRING = AWAIT $task`.
///
/// Shadow-safe by construction: the grammar only routes UPPERCASE-led
/// `instruction` lines here (`let_async_statement` claims ASYNC-led and
/// WITH_IO-led lines first; lowercase/digit/sigil RHSs never match). Rust
/// then branches on the lead token: known commands lower to capture,
/// unknown leads re-parse as plain expressions.
fn parse_let_capture_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    use pest::Parser;
    let mut var = None;
    let mut decl_type: Option<TypeKind> = None;
    let mut await_pair = None;
    let mut timeout_pair = None;
    let mut call_pair = None;
    let mut instruction_pair = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                var = Some(parse_dollar_ident(inner));
            }
            Rule::type_tag => {
                decl_type = Some(parse_type_tag(ctx, inner)?);
            }
            Rule::await_statement => {
                await_pair = Some(inner);
            }
            Rule::timeout_statement => {
                timeout_pair = Some(inner);
            }
            Rule::call_statement => {
                call_pair = Some(inner);
            }
            Rule::instruction => {
                instruction_pair = Some(inner);
            }
            _ => {}
        }
    }
    let var = var.ok_or_else(|| {
        ParseError::validation("LET", "LET requires a variable".to_string(), &span)
    })?;
    let dtype: TypeKind = decl_type.ok_or_else(|| {
        ParseError::validation(
            "LET",
            "LET requires explicit type: LET $var: TYPE = ...".to_string(),
            &span,
        )
    })?;
    if let Some(awaited) = await_pair {
        let mut task_var = None;
        for inner in awaited.into_inner() {
            if inner.as_rule() == Rule::ident {
                task_var = Some(inner.as_str().to_string());
            }
        }
        return Ok(StepKind::AwaitCapture {
            out_var: var,
            out_type: dtype,
            task_var: task_var.ok_or_else(|| {
                ParseError::validation(
                    "LET",
                    "LET $out = AWAIT requires a task variable".to_string(),
                    &span,
                )
            })?,
        });
    }
    if let Some(timeouted) = timeout_pair {
        let kind = parse_structural_command_with_lower(ctx, timeouted, lower)?;
        reject_async_in_capture(ctx, &kind)?;
        reject_pipe_stdout_in_capture(ctx, &kind)?;
        return Ok(StepKind::AssignCapture {
            var,
            decl_type: dtype,
            cmd: Box::new(kind),
        });
    }
    if let Some(called) = call_pair {
        let kind = parse_call_statement_from_pair(ctx, called)?;
        reject_async_in_capture(ctx, &kind)?;
        reject_pipe_stdout_in_capture(ctx, &kind)?;
        return Ok(StepKind::AssignCapture {
            var,
            decl_type: dtype,
            cmd: Box::new(kind),
        });
    }
    if let Some(ins) = instruction_pair {
        let text = ins.as_str().to_string();
        let mut lead = None;
        for token in ins.into_inner() {
            if token.as_rule() == Rule::command_name {
                lead = Some(token.as_str().to_string());
                break;
            }
        }
        let lead = lead.ok_or_else(|| {
            ParseError::validation("LET", "LET capture requires a command".to_string(), &span)
        })?;
        if crate::commands::is_known_command(&lead) {
            let kind = lower_instruction_pair(
                ctx,
                lexer::LanguageParser::parse(Rule::instruction, &text)
                    .map_err(parse_pest_error)?
                    .next()
                    .ok_or_else(|| {
                        ParseError::validation(
                            "LET",
                            "LET capture requires a command".to_string(),
                            &span,
                        )
                    })?,
                lower,
            )?;
            reject_async_in_capture(ctx, &kind)?;
            reject_pipe_stdout_in_capture(ctx, &kind)?;
            return Ok(StepKind::AssignCapture {
                var,
                decl_type: dtype,
                cmd: Box::new(kind),
            });
        }
        let expr = parse_expr_str(&span, &text)?;
        return Ok(StepKind::Assign {
            var,
            decl_type: dtype,
            expr,
        });
    }
    Err(ParseError::structural(
        "let",
        "LET requires a value".to_string(),
        &span,
    ))
}

fn parse_await_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut var = None;
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::ident {
            var = Some(inner.as_str().to_string());
        }
    }
    Ok(StepKind::Await {
        var: var.ok_or_else(|| {
            ParseError::validation("AWAIT", "AWAIT requires a variable".to_string(), &span)
        })?,
    })
}

fn parse_cancel_statement_from_pair(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut var = None;
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::ident {
            var = Some(inner.as_str().to_string());
        }
    }
    Ok(StepKind::Cancel {
        var: var.ok_or_else(|| {
            ParseError::validation("CANCEL", "CANCEL requires a variable".to_string(), &span)
        })?,
    })
}

/// Build a TIMEOUT duration [`Arg`] from the widened `timeout_duration`
/// alternatives. Static literals type-check now via the declared Duration
/// arg type; dynamics (`$var`, templates) resolve at runtime.
fn parse_timeout_duration_arg(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Arg> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        let arg = match inner.as_rule() {
            Rule::timeout_literal => Arg::String(inner.as_str().to_string(), false),
            Rule::dollar_ident => Arg::Expr(Expr::Var(parse_dollar_ident(inner))),
            Rule::quoted_string => Arg::String(
                crate::command::strip_surrounding_quotes(inner.as_str()).to_string(),
                true,
            ),
            Rule::templated_arg => Arg::String(inner.as_str().to_string(), false),
            _ => continue,
        };
        ArgType::Duration
            .check_arg(&arg)
            .map_err(|e| ParseError::validation("TIMEOUT", e.to_string(), &span))?;
        return Ok(arg);
    }
    Err(ParseError::validation(
        "TIMEOUT",
        "TIMEOUT requires a duration".to_string(),
        &span,
    ))
}

fn parse_timeout_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut duration: Option<Arg> = None;
    let mut body: Option<Vec<Step>> = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::timeout_duration => {
                duration = Some(parse_timeout_duration_arg(ctx, inner)?);
            }
            Rule::block => {
                body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
            }
            Rule::await_statement => {
                let kind = parse_await_statement_from_pair(ctx, inner)?;
                body = Some(vec![Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            Rule::cancel_statement => {
                let kind = parse_cancel_statement_from_pair(ctx, inner)?;
                body = Some(vec![Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            Rule::with_io_command
            | Rule::inherit_env_command
            | Rule::async_statement
            | Rule::async_statement_block
            | Rule::call_statement
            | Rule::while_statement
            | Rule::func_def
            | Rule::return_statement
            | Rule::break_statement
            | Rule::continue_statement
            | Rule::timeout_statement => {
                let kind = parse_structural_command_with_lower(ctx, inner, lower)?;
                body = Some(vec![Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            Rule::instruction | Rule::instruction_inner => {
                let kind = lower_instruction_pair(ctx, inner, lower)?;
                body = Some(vec![Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            Rule::run_exec_statement | Rule::run_exec_inner => {
                let kind = lower_run_exec_pair(ctx, inner, lower)?;
                body = Some(vec![Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                }]);
            }
            _ => {}
        }
    }
    Ok(StepKind::Timeout {
        duration: duration.ok_or_else(|| {
            ParseError::validation("TIMEOUT", "TIMEOUT requires a duration".to_string(), &span)
        })?,
        body: body.ok_or_else(|| {
            ParseError::validation(
                "TIMEOUT",
                "TIMEOUT requires a command or block".to_string(),
                &span,
            )
        })?,
    })
}

fn parse_if_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut cond = None;
    let mut then_body = Vec::new();
    let mut else_ifs = Vec::new();
    let mut else_body = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => {
                if cond.is_none() {
                    cond = Some(parse_expr(ctx, inner)?);
                }
            }
            Rule::block => {
                if then_body.is_empty() {
                    then_body = parse_block_elements_with_lower(ctx, inner, lower)?;
                }
            }
            Rule::else_if_clause => {
                let (eif_cond, eif_body) = parse_else_if_clause(ctx, inner, lower)?;
                else_ifs.push((eif_cond, eif_body));
            }
            Rule::else_clause => {
                else_body = Some(parse_else_clause(ctx, inner, lower)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::If {
        cond: Box::new(cond.ok_or_else(|| {
            ParseError::structural("if", "IF requires a condition".to_string(), &span)
        })?),
        then_body,
        else_ifs,
        else_body,
    })
}

fn parse_else_if_clause(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<(Box<Expr>, Vec<Step>)> {
    let span = refine_span(ctx, &pair);
    let mut cond = None;
    let mut body = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => cond = Some(parse_expr(ctx, inner)?),
            Rule::block => body = parse_block_elements_with_lower(ctx, inner, lower)?,
            _ => {}
        }
    }
    Ok((
        Box::new(cond.ok_or_else(|| {
            ParseError::structural("if", "ELSE IF requires a condition".to_string(), &span)
        })?),
        body,
    ))
}

fn parse_else_clause(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<Vec<Step>> {
    for inner in pair.into_inner() {
        if let Rule::block = inner.as_rule() {
            return parse_block_elements_with_lower(ctx, inner, lower);
        }
    }
    Ok(Vec::new())
}

fn parse_async_statement_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut inner_cmd = None;
    let mut block_body = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::command => {
                // command is _{} = silent, so its children aren't visible as pairs
                // when nested inside compound-atomic async_statement.
                // Parse the command text directly.
                let cmd_text = inner.as_str();
                let steps = parse_script(cmd_text, |name, args| lower(name, args))?;
                if steps.len() == 1 {
                    inner_cmd = Some(steps.into_iter().next().unwrap().kind);
                } else {
                    return Err(ParseError::structural(
                        "async",
                        "unexpected multiple steps in async inner command".to_string(),
                        &span,
                    ));
                }
            }
            Rule::command_inner => {
                // command_inner = { inherit_env_command | async_statement | async_statement_block | instruction }
                let child = inner.into_inner().next().ok_or_else(|| {
                    ParseError::structural("async", "empty command_inner".to_string(), &span)
                })?;
                match child.as_rule() {
                    Rule::inherit_env_command => {
                        inner_cmd = Some(parse_structural_command_with_lower(ctx, child, lower)?);
                    }
                    Rule::async_statement | Rule::async_statement_block => {
                        inner_cmd = Some(parse_structural_command_with_lower(ctx, child, lower)?);
                    }
                    Rule::timeout_statement | Rule::cancel_statement => {
                        inner_cmd = Some(parse_structural_command_with_lower(ctx, child, lower)?);
                    }
                    Rule::call_statement | Rule::while_statement => {
                        inner_cmd = Some(parse_structural_command_with_lower(ctx, child, lower)?);
                    }
                    Rule::func_def
                    | Rule::return_statement
                    | Rule::break_statement
                    | Rule::continue_statement => {
                        return Err(ParseError::structural(
                            "async",
                            format!(
                                "{:?} cannot run as a lone ASYNC command; use ASYNC {{ ... }} block form if needed",
                                child.as_rule()
                            ),
                            &span,
                        ));
                    }
                    Rule::instruction => {
                        inner_cmd = Some(lower_instruction_pair(ctx, child, lower)?);
                    }
                    Rule::run_exec_statement | Rule::run_exec_inner => {
                        inner_cmd = Some(lower_run_exec_pair(ctx, child, lower)?);
                    }
                    other => {
                        return Err(ParseError::structural(
                            "async",
                            format!("unexpected command_inner child: {:?}", other),
                            &span,
                        ));
                    }
                }
            }
            Rule::instruction | Rule::instruction_inner => {
                inner_cmd = Some(lower_instruction_pair(ctx, inner, lower)?);
            }
            Rule::run_exec_statement | Rule::run_exec_inner => {
                inner_cmd = Some(lower_run_exec_pair(ctx, inner, lower)?);
            }
            Rule::block => {
                block_body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
            }
            _ => {}
        }
    }
    if let Some(body) = block_body {
        for step in &body {
            if matches!(&step.kind, StepKind::WithIo { .. }) {
                return Err(ParseError::structural("async", "WITH_IO cannot be placed inside ASYNC. Place WITH_IO outside ASYNC instead (e.g. WITH_IO [...] ASYNC RUN ...)".to_string(), &span));
            }
        }
        Ok(StepKind::AsyncBlock { body })
    } else if let Some(cmd) = inner_cmd {
        if matches!(&cmd, StepKind::WithIo { .. }) {
            return Err(ParseError::structural("async", "WITH_IO cannot be placed inside ASYNC. Place WITH_IO outside ASYNC instead (e.g. WITH_IO [...] ASYNC RUN ...)".to_string(), &span));
        }
        Ok(StepKind::AsyncBlock {
            body: vec![Step {
                guard: None,
                kind: cmd,
                scope_enter: 0,
                scope_exit: 0,
            }],
        })
    } else {
        Err(ParseError::structural(
            "async",
            "ASYNC requires either a command or a block".to_string(),
            &span,
        ))
    }
}

fn parse_async_statement_block_from_pair(
    ctx: &SpanContext,
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<StepKind> {
    let span = refine_span(ctx, &pair);
    let mut block_body = None;
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::block {
            block_body = Some(parse_block_elements_with_lower(ctx, inner, lower)?);
        }
    }
    let body = block_body.ok_or_else(|| {
        ParseError::structural(
            "async",
            "async_statement_block requires a block".to_string(),
            &span,
        )
    })?;
    for step in &body {
        if matches!(&step.kind, StepKind::WithIo { .. }) {
            return Err(ParseError::structural("async", "WITH_IO cannot be placed inside ASYNC. Place WITH_IO outside ASYNC instead (e.g. WITH_IO [...] ASYNC RUN ...)".to_string(), &span));
        }
    }
    Ok(StepKind::AsyncBlock { body })
}

fn parse_block_elements_with_lower(
    ctx: &SpanContext,
    block_pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> ParseResult<StepKind>,
) -> ParseResult<Vec<Step>> {
    let mut steps = Vec::new();
    for elem in block_pair.into_inner() {
        match elem.as_rule() {
            Rule::for_statement
            | Rule::while_statement
            | Rule::func_def
            | Rule::call_statement
            | Rule::return_statement
            | Rule::break_statement
            | Rule::continue_statement
            | Rule::let_statement
            | Rule::mutate_statement
            | Rule::let_async_statement
            | Rule::let_capture_statement
            | Rule::await_statement
            | Rule::cancel_statement
            | Rule::if_statement
            | Rule::async_statement
            | Rule::timeout_statement
            | Rule::async_statement_block => {
                let step_kind = parse_structural_command_with_lower(ctx, elem, lower)?;
                steps.push(Step {
                    guard: None,
                    kind: step_kind,
                    scope_enter: 0,
                    scope_exit: 0,
                });
            }
            Rule::guard_block => {
                let mut guard_pair = None;
                let mut inner_block = None;
                for inner in elem.into_inner() {
                    match inner.as_rule() {
                        Rule::guard_line => guard_pair = Some(inner),
                        Rule::block => inner_block = Some(inner),
                        _ => {}
                    }
                }
                if let (Some(gp), Some(bp)) = (guard_pair, inner_block) {
                    let guard_expr = parse_guard_line(ctx, gp)?;
                    let mut inner_steps = parse_block_elements_with_lower(ctx, bp, lower)?;
                    for step in &mut inner_steps {
                        step.guard = Some(guard_expr.clone());
                    }
                    steps.extend(inner_steps);
                }
            }
            Rule::instruction | Rule::instruction_inner => {
                let kind = lower_instruction_pair(ctx, elem, lower)?;
                steps.push(Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                });
            }
            Rule::run_exec_statement | Rule::run_exec_inner => {
                let kind = lower_run_exec_pair(ctx, elem, lower)?;
                steps.push(Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                });
            }
            Rule::with_io_command => {
                let step_kind = parse_structural_command_with_lower(ctx, elem, lower)?;
                steps.push(Step {
                    guard: None,
                    kind: step_kind,
                    scope_enter: 0,
                    scope_exit: 0,
                });
            }
            _ => {} // blank, hash_comment, semicolon, block_start, block_end, etc.
        }
    }
    Ok(steps)
}

fn parse_argument(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Vec<Arg>> {
    let inners: Vec<_> = pair.into_inner().collect();
    // An `expr` fragment can swallow its trailing separator through inner
    // `gap` rules, gluing following text into one argument pair
    // (`ECHO $x hello` lexes as `[expr("$x "), unquoted("hello")]`). Split
    // groups there so expressions survive as typed `Arg::Expr`; every other
    // fragment kind is whitespace-tight by construction.
    let mut groups: Vec<Vec<Pair<Rule>>> = vec![Vec::new()];
    for fragment in inners {
        let glued = fragment.as_rule() == Rule::expr
            && fragment.as_str().ends_with(|c: char| c.is_whitespace());
        groups
            .last_mut()
            .expect("argument always holds a group")
            .push(fragment);
        if glued {
            groups.push(Vec::new());
        }
    }
    let mut args = Vec::new();
    for group in groups {
        if group.is_empty() {
            continue;
        }
        // Single expression — preserve as Arg::Expr for runtime evaluation
        if group.len() == 1 && group[0].as_rule() == Rule::expr {
            args.push(Arg::Expr(parse_expr(
                ctx,
                group.into_iter().next().expect("group holds one pair"),
            )?));
            continue;
        }
        // Single quoted string: preserve quote status and process escapes
        if group.len() == 1 && group[0].as_rule() == Rule::string_literal {
            args.push(Arg::String(parse_fragments(&group)?, true));
            continue;
        }
        args.push(Arg::String(parse_fragments(&group)?, false));
    }
    Ok(args)
}

fn parse_quoted_string(pair: Pair<Rule>) -> ParseResult<String> {
    let s = pair.as_str();
    let content = &s[1..s.len() - 1];
    // Pass contents verbatim — all escape processing deferred to runtime expand_string
    Ok(content.to_string())
}

/// Concatenate fragment pairs (string_literal, templated_arg, unquoted_arg, expr)
/// into a single String. Adjacent fragments without whitespace are joined directly;
/// fragments separated by whitespace get a space inserted.
fn parse_fragments(parts: &[Pair<Rule>]) -> ParseResult<String> {
    // Single quoted string: unquote unconditionally
    if parts.len() == 1 && parts[0].as_rule() == Rule::string_literal {
        let s = parts[0].as_str();
        return Ok(s[1..s.len() - 1].to_string());
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
            Rule::string_literal => {
                let s = part.as_str();
                let unquoted = &s[1..s.len() - 1];
                body.push_str(unquoted);
            }
            Rule::templated_arg | Rule::unquoted_arg => {
                body.push_str(part.as_str());
            }
            Rule::expr => body.push_str(part.as_str()),
            _ => {}
        }
        last_end = Some(span.end());
    }
    Ok(body)
}

fn parse_guard_line(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(ctx, inner);
        }
    }
    Err(ParseError::structural(
        "guard",
        "guard line missing expression".to_string(),
        &span,
    ))
}

fn parse_io_binding(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<IoBinding> {
    let span = refine_span(ctx, &pair);
    let mut stream = None;
    let mut pipe = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::io_stream => stream = Some(parse_io_stream(inner.as_str())),
            Rule::pipe_binding => pipe = Some(parse_pipe_binding(ctx, inner)?),
            _ => {}
        }
    }
    let stream = stream.ok_or_else(|| {
        ParseError::structural("with_io", "missing IO stream in WITH_IO".to_string(), &span)
    })?;
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

fn parse_pipe_binding(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<PipeTarget> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pipe_name => return Ok(PipeTarget::Name(inner.as_str().to_string())),
            Rule::dollar_ident => {
                return Ok(PipeTarget::Var(parse_dollar_ident(inner)));
            }
            _ => {}
        }
    }
    Err(ParseError::structural(
        "with_io",
        "missing pipe identifier in WITH_IO binding".to_string(),
        &span,
    ))
}

fn parse_guard_expr(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    match pair.as_rule() {
        Rule::guard_expr => {
            let next = pair.into_inner().next().ok_or_else(|| {
                ParseError::structural("guard", "guard expression missing body".to_string(), &span)
            })?;
            parse_guard_expr(ctx, next)
        }
        Rule::guard_seq => parse_guard_seq(ctx, pair),
        Rule::guard_factor => parse_guard_factor(ctx, pair),
        Rule::guard_not => {
            // guard_not is silent, so its inner pairs are the actual content
            Err(ParseError::structural(
                "guard",
                "guard_not should not create a pair".to_string(),
                &span,
            ))
        }
        Rule::guard_primary => parse_guard_primary(ctx, pair),
        Rule::guard_group => parse_guard_group(ctx, pair),
        Rule::guard_any_call => parse_guard_any_call(ctx, pair),
        Rule::guard_all_call => parse_guard_all_call(ctx, pair),
        Rule::not_call => parse_not_call(ctx, pair),
        Rule::guard_term => parse_guard_term(ctx, pair),
        _ => Err(ParseError::structural(
            "guard",
            format!("unexpected guard expression rule: {:?}", pair.as_rule()),
            &span,
        )),
    }
}

fn parse_guard_seq(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    let mut exprs = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_factor {
            exprs.push(parse_guard_factor(ctx, inner)?);
        }
    }
    match exprs.len() {
        0 => Err(ParseError::structural(
            "guard",
            "guard list requires at least one entry".to_string(),
            &span,
        )),
        1 => Ok(exprs.pop().unwrap()),
        _ => Ok(GuardExpr::all(exprs)),
    }
}

fn parse_guard_factor(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    let inner = pair.into_inner().next().ok_or_else(|| {
        ParseError::structural(
            "guard",
            "guard factor missing expression".to_string(),
            &span,
        )
    })?;
    parse_guard_expr(ctx, inner)
}

fn parse_not_call(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(ctx, inner).map(|e| GuardExpr::Not(Box::new(e)));
        }
    }
    Err(ParseError::structural(
        "guard",
        "not() missing expression".to_string(),
        &span,
    ))
}

fn parse_guard_primary(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    match pair.as_rule() {
        Rule::guard_primary => {
            let inner = pair.into_inner().next().ok_or_else(|| {
                ParseError::structural("guard", "guard primary missing body".to_string(), &span)
            })?;
            parse_guard_primary(ctx, inner)
        }
        Rule::guard_group => parse_guard_group(ctx, pair),
        Rule::guard_any_call => parse_guard_any_call(ctx, pair),
        Rule::guard_all_call => parse_guard_all_call(ctx, pair),
        Rule::not_call => parse_not_call(ctx, pair),
        Rule::guard_term => parse_guard_term(ctx, pair),
        _ => Err(ParseError::structural(
            "guard",
            format!("unexpected guard primary rule: {:?}", pair.as_rule()),
            &span,
        )),
    }
}

fn parse_guard_group(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(ctx, inner);
        }
    }
    Err(ParseError::structural(
        "guard",
        "grouped guard missing expression".to_string(),
        &span,
    ))
}

fn parse_guard_any_call(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr_list {
            args = parse_guard_expr_list(ctx, inner)?;
        }
    }
    if args.len() < 2 {
        return Err(ParseError::structural(
            "guard",
            "any(...) requires at least two guard expressions".to_string(),
            &span,
        ));
    }
    Ok(GuardExpr::or(args))
}

fn parse_guard_all_call(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr_list {
            args = parse_guard_expr_list(ctx, inner)?;
        }
    }
    if args.is_empty() {
        return Err(ParseError::structural(
            "guard",
            "all(...) requires at least one guard expression".to_string(),
            &span,
        ));
    }
    Ok(GuardExpr::all(args))
}

fn parse_guard_expr_list(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Vec<GuardExpr>> {
    let mut exprs = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            push_guard_or_args_from_expr(ctx, inner, &mut exprs)?;
        }
    }
    Ok(exprs)
}

fn push_guard_or_args_from_expr(
    ctx: &SpanContext,
    expr_pair: Pair<Rule>,
    exprs: &mut Vec<GuardExpr>,
) -> ParseResult<()> {
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
                exprs.push(parse_guard_factor(ctx, factor)?);
            }
            return Ok(());
        }
    }
    exprs.push(parse_guard_expr(ctx, expr_pair)?);
    Ok(())
}

fn parse_guard_term(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<GuardExpr> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::eq_guard => {
                return Ok(GuardExpr::Predicate(parse_func_guard(inner)?));
            }
            Rule::neq_guard => {
                let guard = parse_func_guard(inner)?;
                return Ok(GuardExpr::Not(Box::new(GuardExpr::Predicate(guard))));
            }
            Rule::bool_guard => {
                let val = inner
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::bool_value)
                    .expect("grammar invariant violated: bool_guard missing bool_value")
                    .as_str()
                    .to_string();
                return Ok(GuardExpr::Predicate(Guard::StaticBool { value: val }));
            }
            Rule::env_guard => {
                return Ok(GuardExpr::Predicate(parse_env_guard(inner)?));
            }
            Rule::bare_guard_ident => {
                let tag = inner.as_str();
                if let Ok(g) = parse_platform_tag(ctx, tag) {
                    return Ok(GuardExpr::Predicate(g));
                }
                return Ok(GuardExpr::Predicate(Guard::EnvExists {
                    key: tag.to_string(),
                }));
            }
            _ => {}
        }
    }
    Err(ParseError::structural(
        "guard",
        "missing guard predicate".to_string(),
        &span,
    ))
}

fn parse_func_guard(pair: Pair<Rule>) -> ParseResult<Guard> {
    let mut key = String::new();
    let mut value = String::new();
    let mut saw_env_prefix = false;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::env_prefix => saw_env_prefix = true,
            Rule::env_key if saw_env_prefix => {
                key = inner.as_str().trim().to_string();
            }
            Rule::bare_guard_value | Rule::quoted_string => {
                value = unquote(inner.as_str().trim()).to_string();
            }
            _ => {}
        }
    }
    Ok(Guard::EnvEquals { key, value })
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
}

fn parse_env_guard(pair: Pair<Rule>) -> ParseResult<Guard> {
    let mut key = String::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::env_key {
            key = inner.as_str().trim().to_string();
        }
    }
    Ok(Guard::EnvExists { key })
}

fn parse_platform_tag(ctx: &SpanContext, tag: &str) -> ParseResult<Guard> {
    let target = match tag.to_ascii_lowercase().as_str() {
        "unix" => PlatformGuard::Unix,
        "windows" => PlatformGuard::Windows,
        "mac" | "macos" => PlatformGuard::Macos,
        "linux" => PlatformGuard::Linux,
        _ => {
            return Err(ParseError::structural(
                "platform",
                format!("unknown platform '{}'", tag),
                ctx,
            ));
        }
    };
    Ok(Guard::Platform { target })
}

fn parse_dollar_ident(pair: Pair<Rule>) -> String {
    // Strip the leading '$' from the identifier
    let s = pair.as_str();
    s.strip_prefix('$').unwrap_or(s).to_string()
}

use crate::ast::{ArithOp, CompareOp, LogicalOp, MathOp, Value};

fn parse_expr(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let expr = parse_expr_inner(ctx, pair)?;
    if matches!(expr, Expr::UnsignedIntBoundary(_)) {
        return Err(ParseError::structural(
            "expr",
            "integer overflow: 9223372036854775808 exceeds i64::MAX".to_string(),
            &span,
        ));
    }
    Ok(expr)
}

fn parse_expr_inner(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::expr_logical_or => parse_expr_logical_or(ctx, inner),
        _ => Err(ParseError::structural(
            "expr",
            format!("unexpected expr rule: {:?}", inner.as_rule()),
            &span,
        )),
    }
}

fn parse_expr_logical_or(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let mut left = parse_expr_logical_and(ctx, inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::or_op => LogicalOp::Or,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!("unexpected operator in logical-or: {:?}", op_pair.as_rule()),
                    &span,
                ));
            }
        };
        let right = parse_expr_logical_and(ctx, inner.next().unwrap())?;
        left = Expr::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_logical_and(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let mut left = parse_expr_comparison(ctx, inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::and_op => LogicalOp::And,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!(
                        "unexpected operator in logical-and: {:?}",
                        op_pair.as_rule()
                    ),
                    &span,
                ));
            }
        };
        let right = parse_expr_comparison(ctx, inner.next().unwrap())?;
        reject_boundary(ctx, &left)?;
        reject_boundary(ctx, &right)?;
        left = Expr::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_comparison(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let left = parse_expr_ordering(ctx, inner.next().unwrap())?;
    if let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::eq_op => CompareOp::Eq,
            Rule::neq_op => CompareOp::Ne,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!("unexpected comparison operator: {:?}", op_pair.as_rule()),
                    &span,
                ));
            }
        };
        let right = parse_expr_ordering(ctx, inner.next().unwrap())?;
        return make_compare(ctx, op, left, right);
    }
    Ok(left)
}

fn parse_expr_ordering(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let left = parse_expr_add_sub(ctx, inner.next().unwrap())?;
    if let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::lt_op => CompareOp::Lt,
            Rule::le_op => CompareOp::Le,
            Rule::gt_op => CompareOp::Gt,
            Rule::ge_op => CompareOp::Ge,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!("unexpected ordering operator: {:?}", op_pair.as_rule()),
                    &span,
                ));
            }
        };
        let right = parse_expr_add_sub(ctx, inner.next().unwrap())?;
        return make_compare(ctx, op, left, right);
    }
    Ok(left)
}

fn parse_expr_add_sub(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let mut left = parse_expr_mul_div(ctx, inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::plus_op => ArithOp::Add,
            Rule::minus_op => ArithOp::Sub,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!("unexpected additive operator: {:?}", op_pair.as_rule()),
                    &span,
                ));
            }
        };
        let right = parse_expr_mul_div(ctx, inner.next().unwrap())?;
        left = make_arith(ctx, op, left, right)?;
    }
    Ok(left)
}

fn parse_expr_mul_div(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut inner = pair.into_inner();
    let mut left = parse_expr_unary(ctx, inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::star_op => ArithOp::Mul,
            Rule::slash_op => ArithOp::Div,
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!(
                        "unexpected multiplicative operator: {:?}",
                        op_pair.as_rule()
                    ),
                    &span,
                ));
            }
        };
        let right = parse_expr_unary(ctx, inner.next().unwrap())?;
        left = make_arith(ctx, op, left, right)?;
    }
    Ok(left)
}

fn parse_expr_unary(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut prefixes = Vec::new();
    let mut atom = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::not_op => prefixes.push(false),
            Rule::neg_op => prefixes.push(true),
            Rule::expr_atom => atom = Some(parse_expr_atom(ctx, inner)?),
            _ => {
                return Err(ParseError::structural(
                    "expr",
                    format!("unexpected unary operand rule: {:?}", inner.as_rule()),
                    &span,
                ));
            }
        }
    }
    let mut expr = atom.ok_or_else(|| {
        ParseError::structural(
            "expr",
            "'!'/'-' requires an expression operand".to_string(),
            &span,
        )
    })?;
    // Innermost prefix is closest to the atom: apply in reverse order.
    for is_neg in prefixes.into_iter().rev() {
        if is_neg {
            expr = apply_unary_neg(ctx, expr)?;
        } else {
            reject_boundary(ctx, &expr)?;
            expr = Expr::Not(Box::new(expr));
        }
    }
    Ok(expr)
}

/// Reject a staged `UnsignedIntBoundary` in any position where unary `-`
/// cannot consume it (every composite constructor calls this on children).
fn reject_boundary(ctx: &SpanContext, expr: &Expr) -> ParseResult<()> {
    if matches!(expr, Expr::UnsignedIntBoundary(_)) {
        return Err(ParseError::structural(
            "expr",
            "integer overflow: 9223372036854775808 exceeds i64::MAX".to_string(),
            ctx,
        ));
    }
    Ok(())
}

/// Apply unary `-`: fold literals, consume the `i64::MIN` boundary, else
/// compile to RPN `Neg` (or AST `0 - x` fallback for non-math operands).
fn apply_unary_neg(ctx: &SpanContext, expr: Expr) -> ParseResult<Expr> {
    match expr {
        Expr::Literal(Value::Int(n)) => match n.checked_neg() {
            Some(v) => Ok(Expr::Literal(Value::Int(v))),
            None => Ok(Expr::CompiledMath(vec![
                MathOp::PushConst(Value::Int(n)),
                MathOp::Neg,
            ])),
        },
        Expr::Literal(Value::Float(f)) => Ok(Expr::Literal(Value::Float(-f))),
        Expr::UnsignedIntBoundary(n) => {
            if n == i64::MAX as u64 + 1 {
                Ok(Expr::Literal(Value::Int(i64::MIN)))
            } else {
                Err(ParseError::structural(
                    "expr",
                    format!("integer overflow: {} exceeds i64::MAX", n),
                    ctx,
                ))
            }
        }
        other => {
            if let Some(mut ops) = expr_to_rpn(&other) {
                ops.push(MathOp::Neg);
                Ok(Expr::CompiledMath(ops))
            } else {
                // Non-math operand (list/map/logical): `0 - x` evaluates via
                // the shared arithmetic helper to a runtime Type Error.
                Ok(Expr::Arithmetic {
                    op: ArithOp::Sub,
                    left: Box::new(Expr::Literal(Value::Int(0))),
                    right: Box::new(other),
                })
            }
        }
    }
}

/// Try parse-time constant folding for binary arithmetic/comparison.
/// Returns `Some(literal)` on success, `None` when not both literals or
/// when the op would error at runtime (div-zero/overflow/non-finite:
/// leave for the RPN evaluator so the error surfaces at runtime).
fn try_fold_arith(op: ArithOp, left: &Expr, right: &Expr) -> Option<Expr> {
    let (Expr::Literal(lv), Expr::Literal(rv)) = (left, right) else {
        return None;
    };
    fold_arith_values(op, lv, rv).map(Expr::Literal)
}

fn fold_arith_values(op: ArithOp, left: &Value, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => {
            let v = match op {
                ArithOp::Add => a.checked_add(*b)?,
                ArithOp::Sub => a.checked_sub(*b)?,
                ArithOp::Mul => a.checked_mul(*b)?,
                ArithOp::Div => a.checked_div(*b)?,
            };
            Some(Value::Int(v))
        }
        (Value::Int(a), Value::Float(b)) => fold_float(op, *a as f64, *b),
        (Value::Float(a), Value::Int(b)) => fold_float(op, *a, *b as f64),
        (Value::Float(a), Value::Float(b)) => fold_float(op, *a, *b),
        _ => None,
    }
}

fn fold_float(op: ArithOp, a: f64, b: f64) -> Option<Value> {
    if !a.is_finite() || !b.is_finite() {
        return None;
    }
    let v = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => {
            if b == 0.0 {
                return None;
            }
            a / b
        }
    };
    if v.is_finite() {
        Some(Value::Float(v))
    } else {
        None
    }
}

fn try_fold_compare(op: CompareOp, left: &Expr, right: &Expr) -> Option<Expr> {
    let (Expr::Literal(lv), Expr::Literal(rv)) = (left, right) else {
        return None;
    };
    match (lv, rv) {
        (Value::Int(a), Value::Int(b)) => {
            let r = match op {
                CompareOp::Eq => a == b,
                CompareOp::Ne => a != b,
                CompareOp::Lt => a < b,
                CompareOp::Le => a <= b,
                CompareOp::Gt => a > b,
                CompareOp::Ge => a >= b,
            };
            Some(Expr::Literal(Value::Bool(r)))
        }
        (Value::Int(_), Value::Float(_))
        | (Value::Float(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_)) => {
            let (af, bf) = (as_f64(lv)?, as_f64(rv)?);
            let r = match op {
                CompareOp::Eq => af == bf,
                CompareOp::Ne => af != bf,
                CompareOp::Lt => af < bf,
                CompareOp::Le => af <= bf,
                CompareOp::Gt => af > bf,
                CompareOp::Ge => af >= bf,
            };
            Some(Expr::Literal(Value::Bool(r)))
        }
        (Value::Bool(a), Value::Bool(b)) => match op {
            CompareOp::Eq => Some(Expr::Literal(Value::Bool(a == b))),
            CompareOp::Ne => Some(Expr::Literal(Value::Bool(a != b))),
            _ => None,
        },
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

fn make_arith(ctx: &SpanContext, op: ArithOp, left: Expr, right: Expr) -> ParseResult<Expr> {
    reject_boundary(ctx, &left)?;
    reject_boundary(ctx, &right)?;
    if let Some(folded) = try_fold_arith(op, &left, &right) {
        return Ok(folded);
    }
    if let (Some(mut lops), Some(mut rops)) = (expr_to_rpn(&left), expr_to_rpn(&right)) {
        lops.append(&mut rops);
        lops.push(match op {
            ArithOp::Add => MathOp::Add,
            ArithOp::Sub => MathOp::Sub,
            ArithOp::Mul => MathOp::Mul,
            ArithOp::Div => MathOp::Div,
        });
        return Ok(Expr::CompiledMath(lops));
    }
    Ok(Expr::Arithmetic {
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn make_compare(ctx: &SpanContext, op: CompareOp, left: Expr, right: Expr) -> ParseResult<Expr> {
    reject_boundary(ctx, &left)?;
    reject_boundary(ctx, &right)?;
    if let Some(folded) = try_fold_compare(op, &left, &right) {
        return Ok(folded);
    }
    if let (Some(mut lops), Some(mut rops)) = (expr_to_rpn(&left), expr_to_rpn(&right)) {
        lops.append(&mut rops);
        lops.push(match op {
            CompareOp::Eq => MathOp::Eq,
            CompareOp::Ne => MathOp::Ne,
            CompareOp::Lt => MathOp::Lt,
            CompareOp::Le => MathOp::Le,
            CompareOp::Gt => MathOp::Gt,
            CompareOp::Ge => MathOp::Ge,
        });
        return Ok(Expr::CompiledMath(lops));
    }
    Ok(Expr::Compare {
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// Convert an operand subtree to flat RPN. Returns `None` for shapes with
/// no RPN encoding (`Not`/`Logical`/`List`/`Map`/stray boundary): callers
/// fall back to AST nodes evaluated recursively.
fn expr_to_rpn(expr: &Expr) -> Option<Vec<MathOp>> {
    match expr {
        Expr::Literal(v) => Some(vec![MathOp::PushConst(v.clone())]),
        Expr::Var(name) => Some(vec![MathOp::LoadVar(name.clone())]),
        Expr::Env(key) => Some(vec![MathOp::LoadEnv(key.clone())]),
        Expr::KeyPath { base, keys } => Some(vec![MathOp::LoadKeyPath {
            base: base.clone(),
            keys: keys.clone(),
        }]),
        Expr::Call { name, args } => {
            if name == "INSPECT" {
                let [arg] = args.as_slice() else {
                    return None;
                };
                if let Expr::Var(var) = arg {
                    return Some(vec![MathOp::Inspect(var.clone())]);
                }
                return None;
            }
            let mut ops = Vec::new();
            for arg in args {
                ops.extend(expr_to_rpn(arg)?);
            }
            ops.push(MathOp::Call {
                name: name.clone(),
                arity: args.len(),
            });
            Some(ops)
        }
        Expr::Arithmetic { op, left, right } => {
            let mut ops = expr_to_rpn(left)?;
            ops.extend(expr_to_rpn(right)?);
            ops.push(match op {
                ArithOp::Add => MathOp::Add,
                ArithOp::Sub => MathOp::Sub,
                ArithOp::Mul => MathOp::Mul,
                ArithOp::Div => MathOp::Div,
            });
            Some(ops)
        }
        Expr::Compare { op, left, right } => {
            let mut ops = expr_to_rpn(left)?;
            ops.extend(expr_to_rpn(right)?);
            ops.push(match op {
                CompareOp::Eq => MathOp::Eq,
                CompareOp::Ne => MathOp::Ne,
                CompareOp::Lt => MathOp::Lt,
                CompareOp::Le => MathOp::Le,
                CompareOp::Gt => MathOp::Gt,
                CompareOp::Ge => MathOp::Ge,
            });
            Some(ops)
        }
        Expr::CompiledMath(ops) => Some(ops.clone()),
        Expr::Not(_) | Expr::Logical { .. } | Expr::List(_) | Expr::Map(_) => None,
        Expr::UnsignedIntBoundary(_) => None,
    }
}

fn parse_expr_atom(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::parenthesized_expr => parse_expr_inner(ctx, inner.into_inner().next().unwrap()),
        Rule::func_call => parse_func_call(ctx, inner),
        Rule::key_path => parse_key_path(ctx, inner),
        Rule::variable => {
            let name = inner.as_str();
            let name = name.strip_prefix('$').unwrap_or(name).to_string();
            Ok(Expr::Var(name))
        }
        Rule::env_read => parse_env_read(ctx, inner).map(Expr::Env),
        Rule::pipe_read => parse_pipe_read(ctx, inner).map(|name| Expr::Literal(Value::Pipe(name))),
        Rule::list_literal => parse_list_literal(ctx, inner),
        Rule::map_literal => parse_map_literal(ctx, inner),
        Rule::string_literal | Rule::quoted_string => {
            let s = parse_quoted_string(inner)?;
            Ok(Expr::Literal(Value::String(s)))
        }
        Rule::numeric_literal => parse_numeric_literal(ctx, inner),
        Rule::bare_word => {
            let s = inner.as_str().to_string();
            match s.as_str() {
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                _ => Ok(Expr::Literal(Value::String(s))),
            }
        }
        _ => Err(ParseError::structural(
            "expr",
            format!("unexpected expression atom rule: {:?}", inner.as_rule()),
            &span,
        )),
    }
}

/// Lower an unsigned `numeric_literal` token. Floats (containing `.`) parse
/// as `f64` (non-finite/overflow bails); integers parse as `u64` so the
/// unsigned half of `i64::MIN` (`9223372036854775808`) stages as
/// `UnsignedIntBoundary` for unary `-` to consume. Larger values bail.
fn parse_numeric_literal(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let text = pair.as_str();
    if text.contains('.') {
        let parsed: f64 = text.parse().map_err(|_| {
            ParseError::structural("expr", format!("invalid float literal {text:?}"), &span)
        })?;
        if !parsed.is_finite() {
            return Err(ParseError::structural(
                "expr",
                format!("invalid float literal {text:?}"),
                &span,
            ));
        }
        return Ok(Expr::Literal(Value::Float(parsed)));
    }
    let digits: u64 = text.parse().map_err(|_| {
        ParseError::structural(
            "expr",
            format!("integer overflow: {text:?} exceeds i64::MAX"),
            &span,
        )
    })?;
    if digits <= i64::MAX as u64 {
        Ok(Expr::Literal(Value::Int(digits as i64)))
    } else if digits == i64::MAX as u64 + 1 {
        Ok(Expr::UnsignedIntBoundary(digits))
    } else {
        Err(ParseError::structural(
            "expr",
            format!("integer overflow: {text:?} exceeds i64::MAX"),
            &span,
        ))
    }
}

fn parse_env_read(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<String> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::env_read_key {
            return Ok(inner.as_str().trim().to_string());
        }
    }
    Err(ParseError::structural(
        "expr",
        "env read requires a key: env:KEY".to_string(),
        &span,
    ))
}

fn parse_pipe_read(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<String> {
    let span = refine_span(ctx, &pair);
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::pipe_name {
            return Ok(inner.as_str().trim().to_string());
        }
    }
    Err(ParseError::structural(
        "expr",
        "pipe read requires a name: pipe:NAME".to_string(),
        &span,
    ))
}

fn parse_key_path(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut base = None;
    let mut keys = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => {
                if base.is_none() {
                    base = Some(inner.as_str().to_string());
                }
            }
            Rule::key_path_segment => {
                keys.push(inner.as_str().to_string());
            }
            _ => {}
        }
    }
    Ok(Expr::KeyPath {
        base: base.ok_or_else(|| {
            ParseError::structural(
                "expr",
                "key path requires a base identifier".to_string(),
                &span,
            )
        })?,
        keys,
    })
}

fn parse_func_call(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut name = None;
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => {
                name = Some(inner.as_str().to_string());
            }
            Rule::expr => {
                let arg = parse_expr_inner(ctx, inner)?;
                reject_boundary(ctx, &arg)?;
                args.push(arg);
            }
            _ => {}
        }
    }
    Ok(Expr::Call {
        name: name.ok_or_else(|| {
            ParseError::structural("expr", "function call requires a name".to_string(), &span)
        })?,
        args,
    })
}

fn parse_list_literal(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let mut items = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::expr {
            let item = parse_expr_inner(ctx, inner)?;
            reject_boundary(ctx, &item)?;
            items.push(item);
        }
    }
    Ok(Expr::List(items))
}

fn parse_map_literal(ctx: &SpanContext, pair: Pair<Rule>) -> ParseResult<Expr> {
    let span = refine_span(ctx, &pair);
    let mut entries = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::map_entry {
            let mut key = String::new();
            let mut value = None;
            for entry_inner in inner.into_inner() {
                match entry_inner.as_rule() {
                    Rule::quoted_string => {
                        key = parse_quoted_string(entry_inner)?;
                    }
                    Rule::bare_word => {
                        key = entry_inner.as_str().to_string();
                    }
                    Rule::expr => {
                        let val = parse_expr_inner(ctx, entry_inner)?;
                        reject_boundary(ctx, &val)?;
                        value = Some(val);
                    }
                    _ => {}
                }
            }
            let val = value.ok_or_else(|| {
                ParseError::structural("expr", "map entry missing value".to_string(), &span)
            })?;
            entries.push((key, val));
        }
    }
    Ok(Expr::Map(entries))
}
