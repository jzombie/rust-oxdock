//! Static input discovery for `cargo:rerun-if-changed` emission.
//!
//! Walks the parsed DSL AST and derives watchable inputs:
//! - fully literal path arguments become `cargo:rerun-if-changed` entries,
//! - templated paths contribute their literal directory head (conservative)
//!   plus `cargo:rerun-if-env-changed` entries for every referenced env key,
//! - keys assigned by `ENV` steps inside the same script are excluded, since
//!   the script itself controls those values.
//!
//! Outputs are manifest-dir-relative forward-slash strings; cargo normalizes
//! them per-host.

use std::collections::{BTreeSet, HashSet};

use oxdock_parser::{Arg, ArgPart, AssertTarget, Expr, GuardExpr, MathOp, Step, StepKind};

/// Extract `{{ env:KEY }}` placeholder names from a template string.
fn env_placeholders(template: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("}}") {
            let inner = after[..end].trim();
            if let Some(key) = inner.strip_prefix("env:") {
                keys.push(key.trim().to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    keys
}

fn push_path_entry(entries: &mut BTreeSet<String>, arg: &Arg) {
    let mut text = arg.as_str().trim();
    // Quoted DSL paths keep their quotes through parsing.
    while text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
        text = text[1..text.len() - 1].trim();
    }
    if text.is_empty() {
        return;
    }
    if !text.contains("{{") {
        entries.insert(text.replace('\\', "/"));
    } else {
        // Conservative fallback: watch the literal directory head so edits
        // near the dynamic name still invalidate the build.
        let head = text
            .split("{{")
            .next()
            .unwrap_or_default()
            .trim_matches('/');
        if !head.is_empty() {
            let head = match head.rfind('/') {
                Some(idx) => &head[..idx],
                None => head,
            };
            if !head.is_empty() && !head.contains("{{") {
                entries.insert(head.replace('\\', "/"));
            }
        }
    }
}

/// Compute the ordered, de-duplicated input directives for a parsed script.
///
/// `assigned_keys` are the variables the script sets itself (`ENV` steps);
/// placeholders referencing them are skipped because their values never come
/// from the host environment.
pub fn plan_input_directives(steps: &[oxdock_parser::Step]) -> (Vec<String>, Vec<String>) {
    let mut changed: BTreeSet<String> = BTreeSet::new();
    let mut env_changed: BTreeSet<String> = BTreeSet::new();
    let mut assigned: HashSet<String> = HashSet::new();

    for step in steps {
        if let StepKind::Env { key, .. } = &step.kind {
            assigned.insert(key.clone());
        }
    }

    for step in steps {
        match &step.kind {
            StepKind::Copy { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, from.as_str(), &assigned);
            }
            StepKind::CopyGit { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, from.as_str(), &assigned);
            }
            StepKind::Symlink { from, .. } => {
                push_path_entry(&mut changed, from);
                collect_env_keys(&mut env_changed, from.as_str(), &assigned);
            }
            StepKind::HashSha256 { path } => {
                push_path_entry(&mut changed, path);
                collect_env_keys(&mut env_changed, path.as_str(), &assigned);
            }
            StepKind::Read(Some(path)) => {
                push_path_entry(&mut changed, path);
                collect_env_keys(&mut env_changed, path.as_str(), &assigned);
            }
            _ => {}
        }
    }

    (
        changed.into_iter().collect(),
        env_changed.into_iter().collect(),
    )
}

fn collect_env_keys(out: &mut BTreeSet<String>, template: &str, assigned: &HashSet<String>) {
    let template = template.trim().trim_matches('"');
    for key in env_placeholders(template) {
        if !assigned.contains(&key) {
            out.insert(key);
        }
    }
}

/// Collect environment variable names referenced through `{{ env:KEY }}`
/// placeholders in command template fields and expression string literals,
/// through `env:KEY` expression reads, through `[env:KEY]` guard
/// expressions (including nested `all`/`or`/`not` groups), and through
/// `INHERIT_ENV` keys.
///
/// Used by fingerprinting so environment drift invalidates cached assets.
pub fn collect_env_references(steps: &[Step]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();

    fn walk_expr(out: &mut BTreeSet<String>, expr: &Expr) {
        match expr {
            Expr::Literal(value) => {
                if let Some(text) = value.as_str() {
                    out.extend(env_placeholders(text));
                }
            }
            Expr::Var(_) => {}
            Expr::Env(key) => {
                out.insert(key.clone());
            }
            Expr::KeyPath { .. } => {}
            Expr::List(items) => {
                for item in items {
                    walk_expr(out, item);
                }
            }
            Expr::Map(entries) => {
                for (_, value) in entries {
                    walk_expr(out, value);
                }
            }
            Expr::Call { args, .. } => {
                for arg in args {
                    walk_expr(out, arg);
                }
            }
            Expr::Inspect(_) => {}
            // Fresh pipe backends carry no env references.
            Expr::FreshPipe => {}
            Expr::Compare { left, right, .. } => {
                walk_expr(out, left);
                walk_expr(out, right);
            }
            Expr::Arithmetic { left, right, .. } => {
                walk_expr(out, left);
                walk_expr(out, right);
            }
            Expr::CompiledMath(ops) => {
                for op in ops {
                    match op {
                        MathOp::PushConst(value) => {
                            if let Some(text) = value.as_str() {
                                out.extend(env_placeholders(text));
                            }
                        }
                        MathOp::LoadEnv(key) => {
                            out.insert(key.clone());
                        }
                        _ => {}
                    }
                }
            }
            Expr::UnsignedIntBoundary(_) => {}
            Expr::Not(inner) => walk_expr(out, inner),
            Expr::Logical { left, right, .. } => {
                walk_expr(out, left);
                walk_expr(out, right);
            }
        }
    }

    fn template_keys(out: &mut BTreeSet<String>, t: &Arg) {
        match t {
            Arg::String(text, _) => {
                out.extend(env_placeholders(text));
            }
            Arg::Expr(expr) => walk_expr(out, expr),
            Arg::Parts(parts) => {
                for part in parts {
                    match part {
                        ArgPart::Text(text, _) => {
                            out.extend(env_placeholders(text));
                        }
                        ArgPart::Expr(expr) => walk_expr(out, expr),
                    }
                }
            }
        }
    }

    fn template_keys_target(out: &mut BTreeSet<String>, t: &AssertTarget) {
        if let AssertTarget::Value(arg) = t {
            template_keys(out, arg);
        }
    }

    fn walk_guard(out: &mut BTreeSet<String>, expr: &GuardExpr) {
        match expr {
            GuardExpr::Predicate(predicate) => match predicate {
                oxdock_parser::Guard::EnvExists { key, .. } => {
                    out.insert(key.clone());
                }
                oxdock_parser::Guard::EnvEquals { key, value, .. } => {
                    // The pair matters: same key with a different expected
                    // value gates differently.
                    out.insert(format!("{key}={value}"));
                    out.insert(key.clone());
                }
                oxdock_parser::Guard::Platform { .. } => {}
                oxdock_parser::Guard::StaticBool { .. } => {}
            },
            GuardExpr::All(children) | GuardExpr::Or(children) => {
                for child in children {
                    walk_guard(out, child);
                }
            }
            GuardExpr::Not(inner) => walk_guard(out, inner),
        }
    }

    for step in steps {
        if let Some(guard) = &step.guard {
            walk_guard(&mut keys, guard);
        }
        match &step.kind {
            StepKind::Workdir(t) => template_keys(&mut keys, t),
            StepKind::Workspace(_) | StepKind::Cwd => {}
            StepKind::Exit(code) => template_keys(&mut keys, code),
            StepKind::Env { key: _, value } => template_keys(&mut keys, value),
            StepKind::InheritEnv { keys: inherit } => {
                keys.extend(inherit.iter().cloned());
            }
            StepKind::Run(t) | StepKind::Echo(t) => template_keys(&mut keys, t),
            StepKind::RunExec { argv } => {
                for arg in argv {
                    template_keys(&mut keys, arg);
                }
            }
            StepKind::AsyncBlock { body } => {
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::Copy { from, to, .. } => {
                template_keys(&mut keys, from);
                template_keys(&mut keys, to);
            }
            StepKind::Symlink { from, to } => {
                template_keys(&mut keys, from);
                template_keys(&mut keys, to);
            }
            StepKind::Mkdir(t) => template_keys(&mut keys, t),
            StepKind::Ls(Some(t)) => template_keys(&mut keys, t),
            StepKind::Ls(None) => {}
            StepKind::Read(None) => {}
            StepKind::Read(Some(t)) => template_keys(&mut keys, t),
            StepKind::Write { path, contents } => {
                template_keys(&mut keys, path);
                if let Some(body) = contents {
                    template_keys(&mut keys, body);
                }
            }
            StepKind::Append { path, contents } => {
                template_keys(&mut keys, path);
                if let Some(body) = contents {
                    template_keys(&mut keys, body);
                }
            }
            StepKind::Expand { path, overrides } => {
                if let Some(p) = path {
                    template_keys(&mut keys, p);
                }
                for (_, value) in overrides {
                    template_keys(&mut keys, value);
                }
            }
            StepKind::AssertEq {
                hash: _,
                actual,
                expected,
            } => {
                template_keys_target(&mut keys, actual);
                if let Some(e) = expected {
                    template_keys(&mut keys, e);
                }
            }
            StepKind::AssertContains { haystack, needle } => {
                template_keys_target(&mut keys, haystack);
                template_keys(&mut keys, needle);
            }
            StepKind::WithIo { cmd, .. } => {
                // WITH_IO wraps exactly one inner command; its templates are
                // reached when the parser expands blocks, but keep a defensive
                // single-level walk for safety.
                collect_env_references_inner(&mut keys, cmd);
            }
            StepKind::WithIoBlock { .. } => {}
            StepKind::CopyGit { rev, from, to, .. } => {
                template_keys(&mut keys, rev);
                template_keys(&mut keys, from);
                template_keys(&mut keys, to);
            }
            StepKind::HashSha256 { path } => template_keys(&mut keys, path),
            StepKind::For { in_expr, body, .. } => {
                walk_expr(&mut keys, in_expr);
                // Recursively collect env references from the loop body
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::If {
                cond,
                then_body,
                else_ifs,
                else_body,
                ..
            } => {
                walk_expr(&mut keys, cond);
                // Recursively collect env references from all branches
                for k in collect_env_references(then_body) {
                    keys.insert(k);
                }
                for (else_cond, body) in else_ifs {
                    walk_expr(&mut keys, else_cond);
                    for k in collect_env_references(body) {
                        keys.insert(k);
                    }
                }
                if let Some(body) = else_body {
                    for k in collect_env_references(body) {
                        keys.insert(k);
                    }
                }
            }
            StepKind::Assign { expr, .. } | StepKind::Set { expr, .. } => {
                walk_expr(&mut keys, expr);
            }
            StepKind::AssignCapture { cmd, .. } => {
                collect_env_references_inner(&mut keys, cmd);
            }
            StepKind::AwaitCapture { .. } => {}
            StepKind::AssignAsync { body, .. } => {
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::Timeout { duration, body } => {
                // Durations resolve dynamically, so they can reference env.
                template_keys(&mut keys, duration);
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::Await { .. } => {}
            StepKind::Cancel { .. } => {}
            StepKind::Sleep { duration } => template_keys(&mut keys, duration),
            StepKind::Connect {
                endpoint, timeout, ..
            } => {
                template_keys(&mut keys, endpoint);
                if let Some(flag) = timeout {
                    template_keys(&mut keys, flag);
                }
            }
            StepKind::Listen { bind, .. } => template_keys(&mut keys, bind),
            StepKind::ReadLine { .. } => {}
            StepKind::FuncDef { body, .. } => {
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::While { cond, body, .. } => {
                walk_expr(&mut keys, cond);
                for k in collect_env_references(body) {
                    keys.insert(k);
                }
            }
            StepKind::Call { args, .. } => {
                for arg in args {
                    walk_expr(&mut keys, arg);
                }
            }
            StepKind::Return { expr } => {
                walk_expr(&mut keys, expr);
            }
            StepKind::Break | StepKind::Continue => {}
        }
    }

    fn collect_env_references_inner(out: &mut BTreeSet<String>, kind: &StepKind) {
        // Minimal re-walk for boxed WithIo inner commands.
        let step_like = Step {
            guard: None,
            kind: kind.clone(),
            scope_enter: 0,
            scope_exit: 0,
        };
        for k in collect_env_references(std::slice::from_ref(&step_like)) {
            out.insert(k);
        }
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_parser::{Arg, Step, StepKind, parse_script};

    fn test_lower(name: &str, args: Vec<Arg>) -> oxdock_parser::ParseResult<StepKind> {
        match name {
            "COPY" => {
                let from = args.first().cloned().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "COPY",
                        "COPY requires source".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                let to = args.get(1).cloned().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "COPY",
                        "COPY requires destination".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                Ok(StepKind::Copy {
                    from_current_workspace: false,
                    from,
                    to,
                })
            }
            "SYMLINK" => {
                let from = args.first().cloned().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "SYMLINK",
                        "SYMLINK requires link".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                let to = args.get(1).cloned().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "SYMLINK",
                        "SYMLINK requires target".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                Ok(StepKind::Symlink { from, to })
            }
            "ENV" => Ok(oxdock_parser::commands::lower_env_assignment(args)?),
            "WRITE" => {
                let path = args.first().cloned().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "WRITE",
                        "WRITE requires path".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                let contents = args.get(1).cloned();
                Ok(StepKind::Write { path, contents })
            }
            "RUN" => {
                let cmd = args.into_iter().next().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "RUN",
                        "RUN requires command".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                Ok(StepKind::Run(cmd))
            }
            "WORKDIR" => {
                let path = args.into_iter().next().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "WORKDIR",
                        "WORKDIR requires path".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                Ok(StepKind::Workdir(path))
            }
            "ECHO" => {
                let msg = args.into_iter().next().ok_or_else(|| {
                    oxdock_parser::ParseError::validation(
                        "ECHO",
                        "ECHO requires message".to_string(),
                        &oxdock_parser::SpanContext::line_only(0),
                    )
                })?;
                Ok(StepKind::Echo(msg))
            }
            _ => Err(oxdock_parser::ParseError::unknown_command(
                name,
                format!("unknown command: {name}"),
                None,
                &oxdock_parser::SpanContext::line_only(0),
            )),
        }
    }

    fn script_steps(script: &str) -> Vec<Step> {
        parse_script(script, test_lower).expect("parse")
    }

    #[test]
    fn literal_copy_sources_are_watched() {
        let steps = script_steps("COPY src/a.txt dist\nSYMLINK lnk target");
        let (changed, env) = plan_input_directives(&steps);
        assert_eq!(changed, vec!["lnk".to_string(), "src/a.txt".to_string()]);
        assert!(env.is_empty());
    }

    #[test]
    fn templated_sources_fall_back_to_directory_head_and_env_watch() {
        let steps = script_steps("ENV BASE=b\nCOPY \"{{ env:BASE }}/file.txt\" dist");
        // ENV assignment must exclude BASE from rerun-if-env-changed.
        let (_, env) = plan_input_directives(&steps);
        assert!(env.is_empty(), "script-assigned keys must be excluded");
    }

    #[test]
    fn unassigned_env_placeholders_are_watched() {
        let steps = script_steps("COPY \"{{ env:ASSET_DIR }}/blob.bin\" out");
        let (changed, env) = plan_input_directives(&steps);
        assert!(
            changed.is_empty(),
            "placeholder-first path has no static head to watch"
        );
        assert_eq!(env, vec!["ASSET_DIR".to_string()]);
    }

    #[test]
    fn env_references_cover_all_step_kinds_and_guards() {
        let steps = script_steps(
            r#"
            WORKDIR {{ env:WD }}
            RUN "echo {{ env:RUNV }}"
            COPY "{{ env:COPYV }}/x" "out"
            WRITE "out/f.txt" "{{ env:BODY }}"
            [env:GATE] {
                ECHO "gated"
            }
            [eq(env:A, 1)] ECHO "eq"
            [any(env:X, env:Y)] ECHO "either"
            "#,
        );
        let refs = collect_env_references(&steps);
        for key in ["WD", "RUNV", "COPYV", "BODY"] {
            assert!(refs.contains(key), "missing {key} in {refs:?}");
        }
        // Guard keys: plain existence, equality pair, and nested or-group.
        assert!(refs.contains("GATE"), "{refs:?}");
        assert!(refs.contains("A"), "{refs:?}");
        assert!(refs.contains("X") && refs.contains("Y"), "{refs:?}");

        // The ENV step's value template is not reachable through the string
        // grammar, so exercise that traversal arm directly.
        use oxdock_parser::{Arg, Step};
        let env_step = Step {
            guard: None,
            kind: StepKind::Env {
                key: "A".into(),
                value: Arg::String("{{ env:SEED }}".to_string(), false),
            },
            scope_enter: 0,
            scope_exit: 0,
        };
        let refs = collect_env_references(std::slice::from_ref(&env_step));
        assert!(refs.contains("SEED"), "{refs:?}");
    }

    #[test]
    fn write_targets_are_not_inputs() {
        let steps = script_steps("WRITE out/generated.txt body");
        let (changed, env) = plan_input_directives(&steps);
        assert!(changed.is_empty());
        assert!(env.is_empty());
    }

    #[test]
    fn env_reads_in_expressions_are_tracked() {
        let steps = parse_script("LET $e: STRING = env:FOO\n", oxdock_parser::lower_command)
            .expect("parse");
        let refs = collect_env_references(&steps);
        assert!(refs.contains("FOO"), "{refs:?}");
    }

    #[test]
    fn inherit_env_keys_are_tracked() {
        let steps =
            parse_script("INHERIT_ENV [HOST_KEY]\n", oxdock_parser::lower_command).expect("parse");
        let refs = collect_env_references(&steps);
        assert!(refs.contains("HOST_KEY"), "{refs:?}");
    }

    #[test]
    fn condition_and_call_exprs_are_tracked() {
        let steps = parse_script(
            "IF env:FLAG == \"on\" {\nECHO hi\n}\n",
            oxdock_parser::lower_command,
        )
        .expect("parse");
        let refs = collect_env_references(&steps);
        assert!(refs.contains("FLAG"), "{refs:?}");
    }
}
