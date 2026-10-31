//! Static call resolution shared by every parse entry point.
//!
//! Pure outcome logic: qualified names out, validation errors otherwise.
//! Callers pass their live scopes; messages live here once, so
//! resolution can never disagree across pathways.

use crate::ast::ModuleTable;
use crate::constants::{
    KEYWORD_EXPORT, KEYWORD_IMPORT, KEYWORD_INSPECT, SCRIPT_MODULE_NAME, qualify, split_qualified,
};
use crate::error::{ParseError, ParseResult, SpanContext};
use std::collections::HashSet;

pub(crate) fn check_func_ident(ctx: &SpanContext, name: &str) -> ParseResult<()> {
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

pub(crate) fn resolve_call_name(
    modules: &ModuleTable,
    funcs: &HashSet<String>,
    imports: &[String],
    ctx: &SpanContext,
    name: &str,
) -> ParseResult<String> {
    if let Some((module, base)) = split_qualified(name) {
        if base == KEYWORD_INSPECT {
            return Err(ParseError::validation(
                "FUNC",
                "INSPECT is a builtin keyword and cannot be module-qualified".to_string(),
                ctx,
            ));
        }
        check_func_ident(ctx, module)?;
        check_func_ident(ctx, base)?;
        match modules.modules.get(module) {
            None => {
                let mut known: Vec<String> = modules.modules.keys().cloned().collect();
                known.sort();
                Err(ParseError::validation(
                    "FUNC",
                    format!(
                        "unknown module `{module}`; known modules: {}",
                        known.join(", ")
                    ),
                    ctx,
                ))
            }
            Some(None) => Ok(name.to_string()),
            Some(Some(funcs)) => {
                if funcs.functions.contains(base) {
                    Ok(qualify(module, base))
                } else {
                    Err(ParseError::validation(
                        "FUNC",
                        format!("unknown function `{module}::{base}`"),
                        ctx,
                    ))
                }
            }
        }
    } else {
        if name == KEYWORD_IMPORT || name == KEYWORD_EXPORT {
            return Err(ParseError::validation(
                "FUNC",
                format!("`{name}` is a directive, not a function"),
                ctx,
            ));
        }
        if name == KEYWORD_INSPECT {
            return Ok(name.to_string());
        }
        if funcs.contains(name) {
            return Ok(qualify(SCRIPT_MODULE_NAME, name));
        }
        // Distinct exporting modules across all visible import frames.
        // Several matches fail instead of shadowing silently: qualify it.
        let mut known_matches: Vec<String> = Vec::new();
        let mut opaque_matches: Vec<String> = Vec::new();
        for module in imports {
            match modules.modules.get(module) {
                Some(Some(funcs)) => {
                    if funcs.functions.contains(name) && !known_matches.contains(module) {
                        known_matches.push(module.clone());
                    }
                }
                Some(None) if !opaque_matches.contains(module) => {
                    opaque_matches.push(module.clone());
                }
                Some(None) | None => {}
            }
        }
        if known_matches.len() > 1 {
            known_matches.sort();
            return Err(ParseError::validation(
                "FUNC",
                format!(
                    "ambiguous function `{name}`: exported by {}; qualify it (e.g. `{}::{name}`)",
                    known_matches.join(", "),
                    known_matches[0],
                ),
                ctx,
            ));
        }
        if let Some(module) = known_matches.pop() {
            return Ok(qualify(&module, name));
        }
        if opaque_matches.len() > 1 {
            opaque_matches.sort();
            return Err(ParseError::validation(
                "FUNC",
                format!(
                    "ambiguous function `{name}`: imported opaque modules {}; qualify it",
                    opaque_matches.join(", "),
                ),
                ctx,
            ));
        }
        if let Some(module) = opaque_matches.pop() {
            return Ok(qualify(&module, name));
        }
        // Nothing in scope: point at the fix when a known module
        // exports the name but was never imported.
        let mut exporters: Vec<String> = modules
            .modules
            .iter()
            .filter_map(|(module, funcs)| match funcs {
                Some(funcs) if funcs.functions.contains(name) => Some(module.clone()),
                _ => None,
            })
            .collect();
        exporters.sort();
        if let Some(first) = exporters.first() {
            return Err(ParseError::validation(
                "FUNC",
                format!(
                    "unknown function `{name}`; qualify it (`{first}::{name}`) or add `IMPORT [{first}]`"
                ),
                ctx,
            ));
        }
        Err(ParseError::validation(
            "FUNC",
            format!("unknown function `{name}`"),
            ctx,
        ))
    }
}
