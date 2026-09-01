#[cfg(not(miri))]
use anyhow::{Context, Result};
#[cfg(not(miri))]
use libtest_mimic::{Arguments, Failed, Trial};
#[cfg(not(miri))]
use oxdock_fs::{EntryKind, PathResolver};
#[cfg(not(miri))]
use oxdock_logic_tests::expectations::{self, ErrorExpectation};
#[cfg(not(miri))]
use oxdock_parser::{Step, script_from_braced_tokens};
#[cfg(not(miri))]
use proc_macro2::TokenStream;
#[cfg(not(miri))]
use std::str::FromStr;

#[cfg(not(miri))]
struct ParityCase {
    name: String,
    dsl: String,
    tokens: String,
    expect_error: Option<ErrorExpectation>,
}

#[cfg(miri)]
fn main() {
    eprintln!("Skipping DSL parity harness under Miri: requires fixture filesystem access.");
}

#[cfg(not(miri))]
fn main() {
    let args = Arguments::from_args();

    let resolver = PathResolver::from_manifest_env().unwrap_or_else(|err| {
        eprintln!("parity harness failed to resolve manifest dir: {err:#}");
        std::process::exit(1);
    });

    let cases = discover_cases(&resolver).unwrap_or_else(|err| {
        eprintln!("parity harness failed to discover cases: {err:#}");
        std::process::exit(1);
    });

    let tests: Vec<Trial> = cases
        .into_iter()
        .map(|case| {
            let name = case.name.clone();
            Trial::test(name, move || run_case(&case))
        })
        .collect();

    libtest_mimic::run(&args, tests).exit();
}

#[cfg(not(miri))]
fn discover_cases(resolver: &PathResolver) -> Result<Vec<ParityCase>> {
    let fixtures_root = resolver.root().join("fixtures")?.join("parity")?;
    let entries = resolver
        .read_dir_entries(&fixtures_root)
        .context("failed to read parity fixtures directory")?;

    let mut cases = Vec::new();
    for entry in entries {
        let file_type = entry
            .file_type()
            .context("failed to read parity entry type")?;
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let case_root = fixtures_root.join(&name)?;
        let dsl = case_root.join("dsl.txt")?;
        let tokens = case_root.join("tokens.rs")?;

        if matches!(resolver.entry_kind(&dsl), Ok(EntryKind::File))
            && matches!(resolver.entry_kind(&tokens), Ok(EntryKind::File))
        {
            let dsl_contents = resolver
                .read_to_string(&dsl)
                .with_context(|| format!("failed to read DSL fixture {name}"))?;
            let token_contents = resolver
                .read_to_string(&tokens)
                .with_context(|| format!("failed to read tokens fixture {name}"))?;
            let expect_error = expectations::load_error_expectation(resolver, &case_root)?;
            cases.push(ParityCase {
                name,
                dsl: dsl_contents,
                tokens: token_contents,
                expect_error,
            });
        }
    }

    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cases)
}

#[cfg(not(miri))]
fn run_case(case: &ParityCase) -> std::result::Result<(), Failed> {
    run_case_inner(case).map_err(|err| Failed::from(err.to_string()))
}

#[cfg(not(miri))]
fn run_case_inner(case: &ParityCase) -> Result<()> {
    let dsl_steps = oxdock_core::parse_script(case.dsl.trim());
    let token_steps = TokenStream::from_str(case.tokens.as_str())
        .map_err(|err| anyhow::anyhow!("failed to parse tokens fixture: {err}"))
        .and_then(|token_stream| script_from_braced_tokens(&token_stream))
        .and_then(|rendered| oxdock_core::parse_script(&rendered));

    if let Some(expected) = case.expect_error.as_ref() {
        let dsl_error = dsl_steps.as_ref().err();
        let token_error = token_steps.as_ref().err();
        if dsl_error.is_none() || token_error.is_none() {
            anyhow::bail!(
                "expected both parsers to error for case {}, but DSL error: {}, token error: {}",
                case.name,
                dsl_error.is_some(),
                token_error.is_some()
            );
        }
        expectations::assert_error_matches(
            expected,
            dsl_error.unwrap(),
            &format!("DSL parser error for case {}", case.name),
        )?;
        expectations::assert_error_matches(
            expected,
            token_error.unwrap(),
            &format!("token parser error for case {}", case.name),
        )?;
        return Ok(());
    }

    let dsl_steps = dsl_steps.context("failed to parse DSL fixture")?;
    let token_steps = token_steps?;

    if dsl_steps != token_steps {
        anyhow::bail!(
            "AST mismatch for case {}.\n\nDSL AST:\n{}\n\nToken AST:\n{}",
            case.name,
            render_steps(&dsl_steps),
            render_steps(&token_steps)
        );
    }

    Ok(())
}

#[cfg(not(miri))]
fn render_steps(steps: &[Step]) -> String {
    steps
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
