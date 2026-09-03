use crate::ast::{Arg, Guard, GuardExpr, IoBinding, IoStream, PlatformGuard, Step, StepKind};
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

pub struct ScriptParser<'a, F: Fn(&str, Vec<Arg>) -> Result<StepKind>> {
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
    lower: F,
}

impl<'a, F: Fn(&str, Vec<Arg>) -> Result<StepKind>> ScriptParser<'a, F> {
    pub fn new(input: &'a str, lower: F) -> Result<Self> {
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
            lower,
        })
    }

    pub fn parse(mut self) -> Result<Vec<Step>> {
        while let Some(token) = self.tokens.pop_front() {
            if self.pending_io_block.is_some()
                && !matches!(
                    token,
                    RawToken::BlockStart { .. }
                        | RawToken::Command { .. }
                        | RawToken::Instruction { .. }
                )
            {
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
                    let kind = parse_structural_command_with_lower(pair, &self.lower)?;
                    self.handle_command_token(line_no, kind)?
                }
                RawToken::Instruction { pair, line_no } => {
                    let kind = self.lower_instruction(pair)?;
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

    fn lower_instruction(&self, pair: Pair<Rule>) -> Result<StepKind> {
        let (name, args) = extract_instruction(pair)?;
        (self.lower)(&name, args)
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

pub fn parse_script(
    input: &str,
    lower: impl Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<Vec<Step>> {
    ScriptParser::new(input, lower)?.parse()
}

pub fn parse_guard_expr_str(input: &str) -> Result<GuardExpr> {
    use pest::Parser;
    let pairs = lexer::LanguageParser::parse(Rule::guard_expr, input)
        .map_err(|e| anyhow!("guard parse error: {e}"))?;
    let pair = pairs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("empty guard"))?;
    parse_guard_expr(pair)
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

fn parse_structural_command_with_lower(
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<StepKind> {
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
                                bindings.push(parse_io_binding(flag)?);
                            }
                        }
                    }
                    Rule::with_io_command => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(inner, lower)?));
                    }
                    Rule::inherit_env_command => {
                        cmd = Some(Box::new(parse_structural_command_with_lower(inner, lower)?));
                    }
                    Rule::instruction | Rule::instruction_inner => {
                        let (name, args) = extract_instruction(inner)?;
                        cmd = Some(Box::new(lower(&name, args)?));
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
        Rule::for_statement => parse_for_statement_from_pair(pair, lower)?,
        Rule::let_statement => parse_let_statement_from_pair(pair)?,
        Rule::if_statement => parse_if_statement_from_pair(pair, lower)?,
        _ => bail!("unexpected structural command rule: {:?}", pair.as_rule()),
    };
    Ok(kind)
}

fn extract_instruction(pair: Pair<Rule>) -> Result<(String, Vec<Arg>)> {
    let mut name = None;
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::command_name => {
                name = Some(inner.as_str().to_string());
            }
            Rule::argument => {
                args.push(parse_argument(inner)?);
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| anyhow!("instruction missing command name"))?;
    Ok((name, args))
}

fn parse_for_statement_from_pair(
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<StepKind> {
    let mut idents = Vec::new();
    let mut in_expr = None;
    let mut body_steps = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                idents.push(parse_dollar_ident(inner));
            }
            Rule::expr => {
                in_expr = Some(parse_expr(inner)?);
            }
            Rule::block => {
                body_steps = parse_block_elements_with_lower(inner, lower)?;
            }
            _ => {}
        }
    }
    let (key_var, var) = match idents.len() {
        1 => (None, idents.into_iter().next().unwrap()),
        2 => {
            let mut iter = idents.into_iter();
            (Some(iter.next().unwrap()), iter.next().unwrap())
        }
        _ => bail!("FOR requires at least one variable"),
    };
    Ok(StepKind::For {
        key_var,
        var,
        in_expr: in_expr.ok_or_else(|| anyhow!("FOR requires an iterable expression"))?,
        body: body_steps,
    })
}

fn parse_let_statement_from_pair(pair: Pair<Rule>) -> Result<StepKind> {
    let mut var = None;
    let mut expr = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::dollar_ident => {
                var = Some(parse_dollar_ident(inner));
            }
            Rule::expr => {
                expr = Some(parse_expr(inner)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::Assign {
        var: var.ok_or_else(|| anyhow!("LET requires a variable"))?,
        expr: expr.ok_or_else(|| anyhow!("LET requires an expression"))?,
    })
}

fn parse_if_statement_from_pair(
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<StepKind> {
    let mut cond = None;
    let mut then_body = Vec::new();
    let mut else_ifs = Vec::new();
    let mut else_body = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => {
                if cond.is_none() {
                    cond = Some(parse_expr(inner)?);
                }
            }
            Rule::block => {
                if then_body.is_empty() {
                    then_body = parse_block_elements_with_lower(inner, lower)?;
                }
            }
            Rule::else_if_clause => {
                let (eif_cond, eif_body) = parse_else_if_clause(inner, lower)?;
                else_ifs.push((eif_cond, eif_body));
            }
            Rule::else_clause => {
                else_body = Some(parse_else_clause(inner, lower)?);
            }
            _ => {}
        }
    }
    Ok(StepKind::If {
        cond: Box::new(cond.ok_or_else(|| anyhow!("IF requires a condition"))?),
        then_body,
        else_ifs,
        else_body,
    })
}

fn parse_else_if_clause(
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<(Box<Expr>, Vec<Step>)> {
    let mut cond = None;
    let mut body = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expr => cond = Some(parse_expr(inner)?),
            Rule::block => body = parse_block_elements_with_lower(inner, lower)?,
            _ => {}
        }
    }
    Ok((
        Box::new(cond.ok_or_else(|| anyhow!("ELSE IF requires a condition"))?),
        body,
    ))
}

fn parse_else_clause(
    pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<Vec<Step>> {
    for inner in pair.into_inner() {
        if let Rule::block = inner.as_rule() {
            return parse_block_elements_with_lower(inner, lower);
        }
    }
    Ok(Vec::new())
}

fn parse_block_elements_with_lower(
    block_pair: Pair<Rule>,
    lower: &dyn Fn(&str, Vec<Arg>) -> Result<StepKind>,
) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for elem in block_pair.into_inner() {
        match elem.as_rule() {
            Rule::for_statement | Rule::let_statement | Rule::if_statement => {
                let step_kind = parse_structural_command_with_lower(elem, lower)?;
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
                    let guard_expr = parse_guard_line(gp)?;
                    let mut inner_steps = parse_block_elements_with_lower(bp, lower)?;
                    for step in &mut inner_steps {
                        step.guard = Some(guard_expr.clone());
                    }
                    steps.extend(inner_steps);
                }
            }
            Rule::instruction | Rule::instruction_inner => {
                let (name, args) = extract_instruction(elem)?;
                let kind = lower(&name, args)?;
                steps.push(Step {
                    guard: None,
                    kind,
                    scope_enter: 0,
                    scope_exit: 0,
                });
            }
            Rule::with_io_command => {
                let step_kind = parse_structural_command_with_lower(elem, lower)?;
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

fn parse_argument(pair: Pair<Rule>) -> Result<Arg> {
    let inner: Vec<_> = pair.into_inner().collect();
    // Single expression — preserve as Arg::Expr for runtime evaluation
    if inner.len() == 1 && inner[0].as_rule() == Rule::expr {
        return Ok(Arg::Expr(parse_expr(inner.into_iter().next().unwrap())?));
    }
    // Single quoted string: preserve quote status and process escapes
    if inner.len() == 1 && inner[0].as_rule() == Rule::string_literal {
        return Ok(Arg::String(parse_fragments(&inner)?, true));
    }
    Ok(Arg::String(parse_fragments(&inner)?, false))
}

fn parse_quoted_string(pair: Pair<Rule>) -> Result<String> {
    let s = pair.as_str();
    let content = &s[1..s.len() - 1];
    // Pass contents verbatim — all escape processing deferred to runtime expand_string
    Ok(content.to_string())
}

/// Concatenate fragment pairs (string_literal, templated_arg, unquoted_arg, expr)
/// into a single String. Adjacent fragments without whitespace are joined directly;
/// fragments separated by whitespace get a space inserted.
fn parse_fragments(parts: &[Pair<Rule>]) -> Result<String> {
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
        Rule::guard_not => {
            // guard_not is silent, so its inner pairs are the actual content
            bail!("guard_not should not create a pair")
        }
        Rule::guard_primary => parse_guard_primary(pair),
        Rule::guard_group => parse_guard_group(pair),
        Rule::guard_any_call => parse_guard_any_call(pair),
        Rule::guard_all_call => parse_guard_all_call(pair),
        Rule::not_call => parse_not_call(pair),
        Rule::guard_term => parse_guard_term(pair),
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
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| anyhow!("guard factor missing expression"))?;
    parse_guard_expr(inner)
}

fn parse_not_call(pair: Pair<Rule>) -> Result<GuardExpr> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr {
            return parse_guard_expr(inner).map(|e| GuardExpr::Not(Box::new(e)));
        }
    }
    bail!("not() missing expression")
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
        Rule::guard_any_call => parse_guard_any_call(pair),
        Rule::guard_all_call => parse_guard_all_call(pair),
        Rule::not_call => parse_not_call(pair),
        Rule::guard_term => parse_guard_term(pair),
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

fn parse_guard_any_call(pair: Pair<Rule>) -> Result<GuardExpr> {
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr_list {
            args = parse_guard_expr_list(inner)?;
        }
    }
    if args.len() < 2 {
        bail!("any(...) requires at least two guard expressions");
    }
    Ok(GuardExpr::or(args))
}

fn parse_guard_all_call(pair: Pair<Rule>) -> Result<GuardExpr> {
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::guard_expr_list {
            args = parse_guard_expr_list(inner)?;
        }
    }
    if args.is_empty() {
        bail!("all(...) requires at least one guard expression");
    }
    Ok(GuardExpr::all(args))
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

fn parse_guard_term(pair: Pair<Rule>) -> Result<GuardExpr> {
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
                if let Ok(g) = parse_platform_tag(tag) {
                    return Ok(GuardExpr::Predicate(g));
                }
                return Ok(GuardExpr::Predicate(Guard::EnvExists {
                    key: tag.to_string(),
                }));
            }
            _ => {}
        }
    }
    bail!("missing guard predicate")
}

fn parse_func_guard(pair: Pair<Rule>) -> Result<Guard> {
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

fn parse_env_guard(pair: Pair<Rule>) -> Result<Guard> {
    let mut key = String::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::env_key {
            key = inner.as_str().trim().to_string();
        }
    }
    Ok(Guard::EnvExists { key })
}

fn parse_platform_tag(tag: &str) -> Result<Guard> {
    let target = match tag.to_ascii_lowercase().as_str() {
        "unix" => PlatformGuard::Unix,
        "windows" => PlatformGuard::Windows,
        "mac" | "macos" => PlatformGuard::Macos,
        "linux" => PlatformGuard::Linux,
        _ => bail!("unknown platform '{}'", tag),
    };
    Ok(Guard::Platform { target })
}

fn parse_dollar_ident(pair: Pair<Rule>) -> String {
    // Strip the leading '$' from the identifier
    let s = pair.as_str();
    s.strip_prefix('$').unwrap_or(s).to_string()
}

use crate::ast::{CompareOp, Expr, LogicalOp, Value};

fn parse_expr(pair: Pair<Rule>) -> Result<Expr> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::expr_logical_or => parse_expr_logical_or(inner),
        _ => bail!("unexpected expr rule: {:?}", inner.as_rule()),
    }
}

fn parse_expr_logical_or(pair: Pair<Rule>) -> Result<Expr> {
    let mut inner = pair.into_inner();
    let mut left = parse_expr_logical_and(inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::or_op => LogicalOp::Or,
            _ => bail!("unexpected operator in logical-or: {:?}", op_pair.as_rule()),
        };
        let right = parse_expr_logical_and(inner.next().unwrap())?;
        left = Expr::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_logical_and(pair: Pair<Rule>) -> Result<Expr> {
    let mut inner = pair.into_inner();
    let mut left = parse_expr_comparison(inner.next().unwrap())?;
    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::and_op => LogicalOp::And,
            _ => bail!(
                "unexpected operator in logical-and: {:?}",
                op_pair.as_rule()
            ),
        };
        let right = parse_expr_comparison(inner.next().unwrap())?;
        left = Expr::Logical {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_expr_comparison(pair: Pair<Rule>) -> Result<Expr> {
    let mut inner = pair.into_inner();
    let left = parse_expr_atom(inner.next().unwrap())?;
    if let Some(op_pair) = inner.next() {
        let op = match op_pair.as_rule() {
            Rule::eq_op => CompareOp::Eq,
            Rule::neq_op => CompareOp::Ne,
            _ => bail!("unexpected comparison operator: {:?}", op_pair.as_rule()),
        };
        let right = parse_expr_atom(inner.next().unwrap())?;
        Ok(Expr::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    } else {
        Ok(left)
    }
}

fn parse_expr_atom(pair: Pair<Rule>) -> Result<Expr> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::parenthesized_expr => parse_expr(inner.into_inner().next().unwrap()),
        Rule::func_call => parse_func_call(inner),
        Rule::key_path => parse_key_path(inner),
        Rule::variable => {
            let name = inner.as_str();
            let name = name.strip_prefix('$').unwrap_or(name).to_string();
            Ok(Expr::Var(name))
        }
        Rule::list_literal => parse_list_literal(inner),
        Rule::map_literal => parse_map_literal(inner),
        Rule::string_literal | Rule::quoted_string => {
            let s = parse_quoted_string(inner)?;
            Ok(Expr::Literal(Value::String(s)))
        }
        Rule::bare_word => {
            let s = inner.as_str().to_string();
            match s.as_str() {
                "true" => Ok(Expr::Literal(Value::Bool(true))),
                "false" => Ok(Expr::Literal(Value::Bool(false))),
                _ => Ok(Expr::Literal(Value::String(s))),
            }
        }
        _ => bail!("unexpected expression atom rule: {:?}", inner.as_rule()),
    }
}

fn parse_key_path(pair: Pair<Rule>) -> Result<Expr> {
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
        base: base.ok_or_else(|| anyhow!("key path requires a base identifier"))?,
        keys,
    })
}

fn parse_func_call(pair: Pair<Rule>) -> Result<Expr> {
    let mut name = None;
    let mut args = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::ident => {
                name = Some(inner.as_str().to_string());
            }
            Rule::expr => {
                args.push(parse_expr(inner)?);
            }
            _ => {}
        }
    }
    Ok(Expr::Call {
        name: name.ok_or_else(|| anyhow!("function call requires a name"))?,
        args,
    })
}

fn parse_list_literal(pair: Pair<Rule>) -> Result<Expr> {
    let mut items = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::expr {
            items.push(parse_expr(inner)?);
        }
    }
    Ok(Expr::List(items))
}

fn parse_map_literal(pair: Pair<Rule>) -> Result<Expr> {
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
                        value = Some(parse_expr(entry_inner)?);
                    }
                    _ => {}
                }
            }
            let val = value.ok_or_else(|| anyhow!("map entry missing value"))?;
            entries.push((key, val));
        }
    }
    Ok(Expr::Map(entries))
}
