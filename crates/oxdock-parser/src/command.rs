use crate::ast::{Arg, Expr, StepKind};
use crate::error::{ParseError, SpanContext};
use anyhow::{Result, anyhow, bail};

/// Metadata for a single command argument.
pub struct ArgSpec {
    pub name: &'static str,
    pub arg_type: ArgType,
    pub description: &'static str,
    pub io: IoDirection,
    pub index: usize,
    pub required: bool,
    pub fallback_stream: Option<Stream>,
}

/// Closed vocabulary for argument value types.
///
/// The closed enum keeps the vocabulary compiler-checked and lets
/// docs-gen link each type cell to its reference section instead of
/// printing bare words like `duration` with no explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    String,
    Path,
    Int,
    Duration,
    Var,
    KeyValue,
    /// Any evaluated value (plus stream markers like `stdout` where the
    /// command accepts them). Renders unlinked as `ANY`.
    Any,
    /// Inline alternation for one-off enums (e.g. `SNAPSHOT|LOCAL`).
    /// Self-describing, so it renders unlinked.
    OneOf(&'static [&'static str]),
    /// Trailing variadic repetition (e.g. `RUN`'s `string...`).
    Rest(&'static ArgType),
}

impl ArgType {
    /// Table-cell label for the argument table's Type column.
    /// Canonical value types use their descriptor names.
    /// Reference and assignment shapes (`$var`, `KEY=value`), inline
    /// alternations, and `ANY` render unlinked. Reference and assignment
    /// shapes display as the `STRING` values they bind or resolve to;
    /// the `$`/assignment requirement itself lives in the argument
    /// description and command syntax.
    pub fn label(&self) -> String {
        match self {
            ArgType::String => "STRING".to_string(),
            ArgType::Path => "PATH".to_string(),
            ArgType::Int => "INT".to_string(),
            ArgType::Duration => "DURATION".to_string(),
            ArgType::Var => "STRING".to_string(),
            ArgType::KeyValue => "STRING".to_string(),
            ArgType::Any => "ANY".to_string(),
            ArgType::OneOf(options) => options.join("|"),
            ArgType::Rest(inner) => format!("{}...", inner.label()),
        }
    }

    /// Anchor of the type's reference section. Only types with a value-type
    /// reference section link; argument shapes (`$var`, `KEY=value`), inline
    /// alternations, and `ANY` render unlinked.
    pub fn anchor(&self) -> Option<String> {
        match self {
            ArgType::String => Some(crate::value::type_anchor("STRING")),
            ArgType::Path => Some(crate::value::type_anchor("PATH")),
            ArgType::Int => Some(crate::value::type_anchor("INT")),
            ArgType::Duration => Some(crate::value::type_anchor("DURATION")),
            ArgType::Var => None,
            ArgType::KeyValue => None,
            ArgType::Any => None,
            ArgType::OneOf(_) => None,
            ArgType::Rest(inner) => inner.anchor(),
        }
    }

    /// Validate a statically-known literal against this type.
    /// Templates and variables are never passed here — see `check_arg`.
    pub fn validate_literal(&self, literal: &str) -> Result<()> {
        match self {
            ArgType::String | ArgType::Path | ArgType::Any => Ok(()),
            ArgType::Int => literal
                .parse::<i64>()
                .map(|_| ())
                .map_err(|_| anyhow!("expected int, got {literal:?}")),
            ArgType::Duration => parse_duration(literal).map(|_| ()),
            ArgType::Var => {
                if literal.starts_with('$') {
                    Ok(())
                } else {
                    bail!("expected $var, got {literal:?}")
                }
            }
            ArgType::KeyValue => match split_assignment(literal)? {
                Some(_) => Ok(()),
                None => bail!("expected KEY=value, got {literal:?}"),
            },
            ArgType::OneOf(options) => {
                // Match the lower-time normalization: bare lowercase
                // spellings are accepted alongside exact options.
                if options
                    .iter()
                    .any(|o| *o == literal || o.to_lowercase() == literal)
                {
                    Ok(())
                } else {
                    bail!("expected one of {}, got {literal:?}", options.join("|"))
                }
            }
            ArgType::Rest(inner) => inner.validate_literal(literal),
        }
    }

    /// Classify one positional arg for lower-time checking.
    /// `Static` literals validate now; templates, variables (except a
    /// `$var` where `Var` is required), and mixed fragments defer to the
    /// runtime resolvers, which see interpolated values.
    pub fn check_arg(&self, arg: &Arg) -> Result<CheckOutcome> {
        match arg {
            Arg::String(s, _) if !s.contains("{{") => {
                self.validate_literal(s)?;
                Ok(CheckOutcome::Static)
            }
            Arg::String(_, _) => Ok(CheckOutcome::Deferred),
            Arg::Parts(_) => Ok(CheckOutcome::Deferred),
            Arg::Expr(Expr::Var(_)) => {
                if *self == ArgType::Var {
                    Ok(CheckOutcome::Static)
                } else {
                    Ok(CheckOutcome::Deferred)
                }
            }
            Arg::Expr(_) => {
                if *self == ArgType::Var {
                    bail!("expected $var, got expression {}", arg.render())
                } else {
                    Ok(CheckOutcome::Deferred)
                }
            }
        }
    }
}

/// Lower-time checking outcome for one positional arg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Validated now; nothing deferred.
    Static,
    /// Unknowable until runtime (template, variable, or fragment);
    /// runtime resolvers enforce the type on the resolved value.
    Deferred,
}

/// Validate positional args against a command's declared specs.
/// Missing required positionals, statically-known type violations, and
/// trailing positionals beyond a fixed-arity spec all fail here;
/// templates, variables, and fragments defer to the runtime resolvers.
/// A trailing `Rest` spec absorbs any number of positionals.
pub fn validate_positionals_against_meta(
    cmd_name: &str,
    specs: &[ArgSpec],
    args: &[Arg],
) -> Result<(), ParseError> {
    let has_rest = specs
        .last()
        .is_some_and(|s| matches!(s.arg_type, ArgType::Rest(_)));

    if !has_rest && args.len() > specs.len() {
        return Err(ParseError::validation(
            cmd_name,
            format!(
                "invalid syntax for command {cmd_name}: expects at most {} positional argument(s), got {}",
                specs.len(),
                args.len()
            ),
            &SpanContext::line_only(0),
        ));
    }

    for spec in specs {
        if let ArgType::Rest(inner) = spec.arg_type {
            // Variadic tail: every trailing positional checks against
            // the inner type, not just the first.
            let tail = args.get(spec.index..).unwrap_or(&[]);
            if tail.is_empty() && spec.required {
                return Err(ParseError::validation(
                    cmd_name,
                    format!(
                        "invalid syntax for command {cmd_name}: requires argument `{}`",
                        spec.name
                    ),
                    &SpanContext::line_only(0),
                ));
            }
            for arg in tail {
                check_one(cmd_name, spec, inner, arg)?;
            }
            return Ok(());
        }
        match args.get(spec.index) {
            Some(arg) => check_one(cmd_name, spec, &spec.arg_type, arg)?,
            None if spec.required => {
                return Err(ParseError::validation(
                    cmd_name,
                    format!(
                        "invalid syntax for command {cmd_name}: requires argument `{}`",
                        spec.name
                    ),
                    &SpanContext::line_only(0),
                ));
            }
            None => {}
        }
    }
    Ok(())
}

fn check_one(
    cmd_name: &str,
    spec: &ArgSpec,
    arg_type: &ArgType,
    arg: &Arg,
) -> Result<(), ParseError> {
    match arg_type.check_arg(arg) {
        Ok(_) => Ok(()),
        Err(e) => Err(ParseError::validation(
            cmd_name,
            format!(
                "invalid syntax for command {cmd_name}: argument `{}` got {} — {e:#}",
                spec.name,
                arg.render()
            ),
            &SpanContext::line_only(0),
        )),
    }
}

/// Strip one layer of surrounding `"` or `'` quotes (both kinds, everywhere).
pub fn strip_surrounding_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(value)
}

/// Single-token `KEY=value` split for direct `lower_command` callers and
/// exotic keys the grammar cannot classify (single tokens only — no whitespace
/// reassembly, so the quoted-space corruption class cannot arise here).
/// Returns `Ok(None)` when there is no `=`.
pub fn split_assignment(text: &str) -> Result<Option<(String, Arg)>> {
    let Some((key, raw)) = text.split_once('=') else {
        return Ok(None);
    };
    if key.is_empty() {
        bail!("assignment requires KEY=value format");
    }
    Ok(Some((
        key.to_string(),
        Arg::String(strip_surrounding_quotes(raw).to_string(), false),
    )))
}

/// Parse a TIMEOUT duration token (`500ms`, `10s`, `2m`, `1h`; a bare
/// number means seconds).
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let (digits, unit_ms): (&str, u64) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60_000)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3_600_000)
    } else {
        (s, 1_000)
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow!("invalid TIMEOUT duration: {s}"))?;
    let millis = n
        .checked_mul(unit_ms)
        .ok_or_else(|| anyhow!("TIMEOUT duration out of range: {s}"))?;
    if millis == 0 {
        bail!("TIMEOUT duration must be positive, got: {s}");
    }
    Ok(std::time::Duration::from_millis(millis))
}

/// Canonical display for a duration: largest exact unit (`500ms`, `10s`,
/// `2m`, `1h`), falling back to milliseconds. Round-trips through
/// [`parse_duration`].
pub fn format_duration(d: &std::time::Duration) -> String {
    let millis = d.as_millis();
    if millis.is_multiple_of(3_600_000) {
        format!("{}h", millis / 3_600_000)
    } else if millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1_000) {
        format!("{}s", millis / 1_000)
    } else {
        format!("{millis}ms")
    }
}

/// Data direction for an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDirection {
    Read,
    Write,
}

/// Stream type for fallback or default output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

/// Metadata for a single flag.
pub struct FlagSpec {
    pub name: &'static str,
    pub long: &'static str,
    pub value_type: FlagValueType,
    pub required: bool,
    pub description: &'static str,
}

/// Type of value a flag accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagValueType {
    /// Boolean flag (no value required).
    Flag,
    /// String-valued flag.
    String,
    /// Integer-valued flag.
    Int,
}

impl FlagValueType {
    /// Display label using the type vocabulary. A bare `Flag`
    /// switch carries no value; `BOOL` names what its presence asserts.
    pub fn label(&self) -> &'static str {
        match self {
            FlagValueType::Flag => "BOOL",
            FlagValueType::String => "STRING",
            FlagValueType::Int => "INT",
        }
    }
}

/// Complete metadata for a command.
pub struct CommandMeta {
    pub name: &'static str,
    pub syntax: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub args: &'static [ArgSpec],
    pub flags: &'static [FlagSpec],
    pub default_output: Option<Stream>,
    pub examples: &'static [Example],
}

/// An executable example for a command.
pub struct Example {
    pub name: &'static str,
    pub fence_meta: Option<&'static str>,
    pub code: &'static str,
}

/// Trait for command metadata and lowering. No execution types.
///
/// This trait lives in `oxdock-parser` and has zero dependencies on
/// `oxdock-core`. Execution dispatch is handled separately by the
/// `define_pipeline!` macro in `oxdock-core`.
pub trait CommandSpec {
    const NAME: &'static str;

    fn metadata() -> CommandMeta;
    fn lower(flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Expr;

    fn lit(text: &str) -> Arg {
        Arg::String(text.to_string(), false)
    }

    fn var(name: &str) -> Arg {
        Arg::Expr(Expr::Var(name.to_string()))
    }

    #[test]
    fn validate_literal_covers_each_variant() {
        ArgType::String.check_arg(&lit("anything at all")).unwrap();
        ArgType::Path.check_arg(&lit("a/b/../c")).unwrap();
        ArgType::Int.check_arg(&lit("3")).unwrap();
        assert!(ArgType::Int.check_arg(&lit("banana")).is_err());
        ArgType::Duration.check_arg(&lit("10s")).unwrap();
        ArgType::Duration.check_arg(&lit("30")).unwrap();
        assert!(ArgType::Duration.check_arg(&lit("banana")).is_err());
        assert!(ArgType::Duration.check_arg(&lit("0s")).is_err());
        ArgType::Var.check_arg(&lit("$x")).unwrap();
        assert!(ArgType::Var.check_arg(&lit("x")).is_err());
        ArgType::KeyValue.check_arg(&lit("K=v")).unwrap();
        ArgType::KeyValue.check_arg(&lit("K=a=b")).unwrap();
        assert!(ArgType::KeyValue.check_arg(&lit("no-equals")).is_err());
        assert!(ArgType::KeyValue.check_arg(&lit("=v")).is_err());
        ArgType::OneOf(&["SNAPSHOT", "LOCAL"])
            .check_arg(&lit("LOCAL"))
            .unwrap();
        // Lowercase spellings stay accepted (WORKSPACE parity).
        ArgType::OneOf(&["SNAPSHOT", "LOCAL"])
            .check_arg(&lit("local"))
            .unwrap();
        assert!(
            ArgType::OneOf(&["SNAPSHOT", "LOCAL"])
                .check_arg(&lit("REMOTE"))
                .is_err()
        );
    }

    #[test]
    fn check_arg_defers_dynamics_and_enforces_var() {
        // Templates defer: their values only exist after interpolation.
        assert_eq!(
            ArgType::Duration.check_arg(&lit("{{ $d }}")).unwrap(),
            CheckOutcome::Deferred
        );
        // Variables satisfy Var statically and defer for everything else.
        assert_eq!(
            ArgType::Var.check_arg(&var("x")).unwrap(),
            CheckOutcome::Static
        );
        assert_eq!(
            ArgType::Duration.check_arg(&var("d")).unwrap(),
            CheckOutcome::Deferred
        );
        // Non-variable expressions where Var is required fail at lower.
        let list = Arg::Expr(Expr::List(vec![]));
        assert!(ArgType::Var.check_arg(&list).is_err());
        assert_eq!(
            ArgType::String.check_arg(&list).unwrap(),
            CheckOutcome::Deferred
        );
    }

    fn spec(index: usize, required: bool, arg_type: ArgType) -> ArgSpec {
        ArgSpec {
            name: "p",
            arg_type,
            description: "",
            io: IoDirection::Write,
            index,
            required,
            fallback_stream: None,
        }
    }

    #[test]
    fn positionals_enforce_required_and_rest_tails() {
        let specs = [spec(0, true, ArgType::Int)];
        assert!(validate_positionals_against_meta("T", &specs, &[]).is_err());
        assert!(validate_positionals_against_meta("T", &specs, &[lit("3")]).is_ok());
        assert!(validate_positionals_against_meta("T", &specs, &[lit("banana")]).is_err());

        // Rest validates EVERY trailing positional, not just the first.
        let specs = [ArgSpec {
            arg_type: ArgType::Rest(&ArgType::Int),
            ..spec(0, true, ArgType::Int)
        }];
        assert!(
            validate_positionals_against_meta("T", &specs, &[lit("1"), lit("2"), lit("banana")])
                .is_err()
        );
        assert!(validate_positionals_against_meta("T", &specs, &[lit("1"), lit("2")]).is_ok());
        // Extras beyond fixed-arity specs fail (no silent truncation).
        let specs = [spec(0, true, ArgType::Int)];
        let err = validate_positionals_against_meta("T", &specs, &[lit("1"), lit("extra")])
            .expect_err("extras must fail");
        assert!(
            err.to_string().contains("at most 1"),
            "unexpected error: {err:#}"
        );
    }
}
