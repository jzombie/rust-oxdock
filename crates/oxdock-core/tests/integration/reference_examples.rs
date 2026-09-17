//! Reference examples stay parseable against the real `STD` table.
//!
//! This lives in `oxdock-core` (not the parser crate) on purpose: builtin
//! membership is owned by the `#[oxdock_func]` registry here, so the parse
//! table derives from `std_module_table()` with no hardcoded mirror. A
//! builtin added, removed, or renamed flows through automatically; an
//! example calling a removed builtin fails loudly right here.

use oxdock_core::{StepKind, all_metadata, parse_script_with_modules, std_module_table};
use oxdock_parser::Step;

#[test]
fn verify_display_sync_with_metadata() {
    fn step_contains_kind(kind: &StepKind, name: &str) -> bool {
        if kind.to_string().starts_with(name) {
            return true;
        }
        let bodies: Vec<&Vec<Step>> = match kind {
            StepKind::For { body, .. }
            | StepKind::While { body, .. }
            | StepKind::FuncDef { body, .. }
            | StepKind::Timeout { body, .. }
            | StepKind::AssignAsync { body, .. }
            | StepKind::AsyncBlock { body } => vec![body],
            StepKind::If {
                then_body,
                else_ifs,
                else_body,
                ..
            } => {
                let mut out = vec![then_body];
                out.extend(else_ifs.iter().map(|(_, b)| b));
                out.extend(else_body.iter());
                out
            }
            _ => {
                if let StepKind::WithIo { cmd, .. } = kind {
                    return step_contains_kind(cmd, name);
                }
                if let StepKind::AssignCapture { cmd, .. } = kind {
                    return step_contains_kind(cmd, name);
                }
                return false;
            }
        };
        bodies
            .iter()
            .any(|body| body.iter().any(|s| step_contains_kind(&s.kind, name)))
    }

    let registry = all_metadata();
    for meta in registry {
        if meta.examples.is_empty() {
            continue;
        }

        let code = meta.examples[0].code;
        let ast = parse_script_with_modules(code, std_module_table())
            .unwrap_or_else(|e| panic!("Failed to parse example for {}: {}", meta.name, e));

        let matching = ast.iter().find(|step| {
            // Mutation has no keyword: its Display (`$var = ...`) cannot
            // start with the metadata name, so match the variant directly.
            if meta.name == "MUTATION" {
                return matches!(step.kind, StepKind::Set { .. });
            }
            // IMPORT is a lowering directive with zero runtime steps:
            // its example proves the import works if a later step
            // resolved through it (checked below), not via Display.
            if meta.name == "IMPORT" {
                return false;
            }
            // Control-flow leaves (RETURN/BREAK/CONTINUE) only occur
            // nested inside bodies, so search recursively; everything
            // else must appear at top level with a matching Display.
            if matches!(meta.name, "RETURN" | "BREAK" | "CONTINUE") {
                return step_contains_kind(&step.kind, meta.name);
            }
            let kind = match &step.kind {
                StepKind::WithIo { cmd, .. } => &**cmd,
                other => other,
            };
            // Full Display covers wrapper kinds themselves (e.g. a
            // WithIo step displays as WITH_IO ...); unwrapped covers
            // wrapped leaf commands.
            kind.to_string().starts_with(meta.name) || step.kind.to_string().starts_with(meta.name)
        });

        assert!(
            matching.is_some() || meta.name == "IMPORT",
            "No step in example for {} produces Display starting with {}",
            meta.name,
            meta.name
        );
        // IMPORT emits no steps: assert its example actually imported
        // by checking the GLOB call resolved qualified.
        if meta.name == "IMPORT" {
            assert!(
                ast.iter()
                    .any(|step| step.to_string().contains("STD::GLOB")),
                "IMPORT example must resolve GLOB through the import"
            );
        }
    }
}
