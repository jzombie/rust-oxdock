//! OxDock's primary user-facing contract: declarative build scripts written
//! inline, directly alongside Rust code.
//!
//! ```rust,ignore
//! use oxdock_macros::oxdock_embed;
//!
//! oxdock_embed! {
//!     name: DemoAssets,
//!     script: {
//!         WORKDIR /
//!         WRITE hello.txt hi
//!     },
//!     out_dir: "prebuilt",
//! }
//! ```
//!
//! The DSL executes through the hardened guarded engine (`oxdock-core` +
//! `oxdock-fs`): every path stays sandboxed, subprocess lifecycle is tracked,
//! and panics are isolated into `compile_error!` streams.
//!
//! Caching: a content fingerprint of the script, its statically discoverable
//! inputs, and every referenced environment variable is stored in
//! `<out_dir>/.oxdock_hash`. Matching fingerprints skip re-execution entirely;
//! any drift rebuilds. `oxdock_prepare!` behaves identically but emits no runtime
//! module.

use oxdock_build::{
    asset_input_fingerprint, embed_debug_enabled, embed_force_rebuild, execution_is_skipped,
    stage_materialize,
};
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_embed::{emit_embed_module, gather_assets, runtime_support_tokens};
#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::{DslMacroInput, ScriptSource, parse_script};
use oxdock_process::BuiltinEnv;
use proc_macro::TokenStream;
use proc_macro2::TokenTree;
use quote::quote;
use syn::parse_macro_input;

/// Runs the DSL at compile-time, materializes assets, and emits a lightweight
/// struct with embedded bytes pointing at the output directory.
///
/// See the crate docs for the full inline contract.
#[proc_macro]
pub fn oxdock_embed(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DslMacroInput);
    expand_embed_tokens(&input).into()
}

/// Executes the DSL like [`oxdock_embed`] but emits no runtime module. Use this when
/// the assets only need to exist during the build.
#[proc_macro]
pub fn oxdock_prepare(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DslMacroInput);
    match expand_prepare_internal(&input) {
        Ok(()) => TokenStream::new(),
        Err(err) => err.to_compile_error().into(),
    }
}

const HASH_FILE: &str = ".oxdock_hash";
const STAGING_DIR: &str = ".oxdock-staging";

/// True when an internal artifact (`hash` marker / staging residue / sandbox
/// markers) would otherwise leak into embedded assets.
fn is_internal_artifact(rel_path: &str) -> bool {
    rel_path == HASH_FILE
        || rel_path.starts_with(STAGING_DIR)
        || rel_path.starts_with(".oxdock-tempdir")
}

fn join_guard(base: &GuardedPath, rel: &str, span: proc_macro2::Span) -> syn::Result<GuardedPath> {
    base.join(rel)
        .map_err(|e| syn::Error::new(span, e.to_string()))
}

fn embed_module_ident(name: &syn::Ident) -> syn::Ident {
    syn::Ident::new(
        &format!("__oxdock_embed_{}", name),
        proc_macro2::Span::call_site(),
    )
}

fn embed_error_stub(name: &syn::Ident) -> proc_macro2::TokenStream {
    let mod_ident = embed_module_ident(name);
    let runtime_support = runtime_support_tokens();
    quote! {
        #[allow(clippy::disallowed_methods, clippy::disallowed_types, non_snake_case)]
        pub mod #mod_ident {
            #runtime_support

            pub struct #name;

            impl #name {
                pub fn get(
                    _file: &str,
                ) -> Option<EmbeddedFile> {
                    None
                }

                pub fn iter() -> Filenames {
                    static EMPTY: [&str; 0] = [];
                    Filenames::from_slice(&EMPTY)
                }
            }
        }

        pub use #mod_ident::#name;
    }
}

fn expand_embed_tokens(input: &DslMacroInput) -> proc_macro2::TokenStream {
    match expand_embed_internal(input) {
        Ok(ts) => ts,
        Err(err) => {
            let compile_error = err.to_compile_error();
            let stub = embed_error_stub(&input.name);
            quote! {
                #compile_error
                #stub
            }
        }
    }
}

/// Extracts DSL text plus a representative span from either literal or braced
/// script forms.
fn script_source_text(
    source: &ScriptSource,
    fallback_span: proc_macro2::Span,
) -> syn::Result<(String, proc_macro2::Span)> {
    match source {
        ScriptSource::Literal(lit) => Ok((lit.value(), lit.span())),
        ScriptSource::Braced(ts) => {
            let text = oxdock_parser::script_from_braced_tokens(ts)
                .map_err(|e| syn::Error::new(fallback_span, format!("parse error: {e}")))?;
            Ok((text, fallback_span))
        }
    }
}

/// Runs `f`, converting panics from the asset engine into `syn::Error`s so a
/// bug can never unwind the host compiler process.
fn catch_engine_panics<T>(
    span: proc_macro2::Span,
    f: impl FnOnce() -> syn::Result<T>,
) -> syn::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(syn::Error::new(
                span,
                format!("asset engine panicked: {msg}"),
            ))
        }
    }
}

struct InlinePlan {
    script_src: String,
    script_span: proc_macro2::Span,
    manifest_resolver: PathResolver,
    out_dir: GuardedPath,
    fingerprint: String,
}

fn prepare_inline_plan(input: &DslMacroInput) -> syn::Result<InlinePlan> {
    let (script_src, script_span) =
        script_source_text(&input.script, proc_macro2::Span::call_site())?;
    let manifest_resolver = PathResolver::from_manifest_env()
        .map_err(|e| syn::Error::new(script_span, e.to_string()))?;
    let out_dir = join_guard(
        manifest_resolver.root(),
        &input.out_dir.value(),
        input.out_dir.span(),
    )?;

    let steps = parse_script(&script_src)
        .map_err(|e| syn::Error::new(script_span, format!("parse error: {e}")))?;
    let build_context = oxdock_fs::discover_workspace_root()
        .map_err(|e| syn::Error::new(script_span, e.to_string()))?;
    let envs = BuiltinEnv::collect(&build_context).into_envs();
    let fingerprint = catch_engine_panics(script_span, || {
        asset_input_fingerprint(
            &manifest_resolver,
            &script_src,
            &steps,
            &out_dir.display().to_string(),
            &envs,
        )
        .map_err(|e| syn::Error::new(script_span, format!("fingerprint failed: {e:#}")))
    })?;

    Ok(InlinePlan {
        script_src,
        script_span,
        manifest_resolver,
        out_dir,
        fingerprint,
    })
}

/// Force lever wins over cache validity: `OXDOCK_EMBED_FORCE_REBUILD`
/// short-circuits the `.oxdock_hash` comparison entirely.
fn should_rebuild(force: bool, cache_valid: bool) -> bool {
    force || !cache_valid
}

fn cached_out_dir_valid(plan: &InlinePlan) -> bool {
    if plan.out_dir.as_path().exists() && !plan.out_dir.as_path().is_dir() {
        return false;
    }
    let hash_path = match plan.out_dir.join(HASH_FILE) {
        Ok(path) => path,
        Err(_) => return false,
    };
    plan.manifest_resolver
        .read_to_string(&hash_path)
        .map(|contents| contents.trim() == plan.fingerprint)
        .unwrap_or(false)
}

fn record_cache_hash(plan: &InlinePlan) -> syn::Result<()> {
    let hash_path = plan
        .out_dir
        .join(HASH_FILE)
        .map_err(|e| syn::Error::new(plan.script_span, e.to_string()))?;
    plan.manifest_resolver
        .write_file(&hash_path, plan.fingerprint.as_bytes())
        .map_err(|e| {
            syn::Error::new(
                plan.script_span,
                format!("failed to record cache hash {}: {e}", hash_path.display()),
            )
        })
}

fn expand_prepare_internal(input: &DslMacroInput) -> syn::Result<()> {
    if execution_is_skipped() {
        return Ok(());
    }
    let plan = catch_engine_panics(proc_macro2::Span::call_site(), || {
        prepare_inline_plan(input)
    })?;

    if should_rebuild(embed_force_rebuild(), cached_out_dir_valid(&plan)) {
        preflight_out_dir_for_build(&plan.out_dir, input.out_dir.span())?;
        catch_engine_panics(plan.script_span, || {
            build_assets(&plan.script_src, plan.script_span, &plan.out_dir)
        })?;
        record_cache_hash(&plan)?;
    }
    Ok(())
}

fn expand_embed_internal(input: &DslMacroInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.name;

    if execution_is_skipped() {
        return Ok(embed_error_stub(name));
    }
    let plan = catch_engine_panics(proc_macro2::Span::call_site(), || {
        prepare_inline_plan(input)
    })?;

    if should_rebuild(embed_force_rebuild(), cached_out_dir_valid(&plan)) {
        preflight_out_dir_for_build(&plan.out_dir, input.out_dir.span())?;
        catch_engine_panics(plan.script_span, || {
            build_assets(&plan.script_src, plan.script_span, &plan.out_dir)
        })?;
        record_cache_hash(&plan)?;
    }

    let mut assets = gather_assets(&plan.manifest_resolver, &plan.out_dir)
        .map_err(|e| syn::Error::new(plan.script_span, e.to_string()))?;
    assets.retain(|asset| !is_internal_artifact(&asset.rel_path));
    emit_embed_module(name, &assets)
}

fn preflight_out_dir_for_build(
    out_dir: &GuardedPath,
    out_dir_span: proc_macro2::Span,
) -> syn::Result<()> {
    // Build a resolver rooted at the manifest; ensure out_dir is created
    let resolver = PathResolver::from_manifest_env()
        .map_err(|e| syn::Error::new(out_dir_span, e.to_string()))?;

    // Ensure out_dir exists
    if out_dir.as_path().exists() {
        if !out_dir.as_path().is_dir() {
            return Err(syn::Error::new(
                out_dir_span,
                format!(
                    "out_dir exists but is not a directory: {}",
                    out_dir.display()
                ),
            ));
        }
    } else {
        resolver.create_dir_all(out_dir).map_err(|e| {
            syn::Error::new(
                out_dir_span,
                format!(
                    "failed to create out_dir {} during pre-check: {e}",
                    out_dir.display()
                ),
            )
        })?;
    }

    // Probe writeability by writing and removing a small file through the resolver.
    let probe = out_dir
        .join(".oxdock_write_probe")
        .map_err(|e| syn::Error::new(out_dir_span, e.to_string()))?;
    match resolver.write_file(&probe, b"") {
        Ok(_) => {
            let _ = resolver.remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(syn::Error::new(
            out_dir_span,
            format!("out_dir not writable: {} ({e})", out_dir.display()),
        )),
    }
}

fn build_assets(
    script: &str,
    span: proc_macro2::Span,
    out_dir: &GuardedPath,
) -> syn::Result<GuardedPath> {
    let debug_embed = embed_debug_enabled();

    // Build in a temp dir; only the final workdir gets materialized into out_dir.
    let tempdir = GuardedPath::tempdir()
        .map_err(|e| syn::Error::new(span, format!("failed to create temp dir: {e}")))?;
    let temp_root_guard = tempdir.as_guarded_path().clone();

    let steps =
        parse_script(script).map_err(|e| syn::Error::new(span, format!("parse error: {e}")))?;

    let resolver =
        PathResolver::from_manifest_env().map_err(|e| syn::Error::new(span, e.to_string()))?;
    let workspace_root =
        oxdock_fs::discover_workspace_root().map_err(|e| syn::Error::new(span, e.to_string()))?;

    let final_cwd = catch_engine_panics(span, || {
        run_steps_with_context_result_with_io(
            &temp_root_guard,
            &workspace_root,
            &steps,
            ExecIo::new(),
        )
        .map_err(|e| {
            // IMPORTANT: Use alternate formatting to include the full error chain and filesystem snapshot.
            syn::Error::new(span, format!("execution error: {e:#}"))
        })
    })?;

    if debug_embed {
        eprintln!(
            "oxdock: build_assets script ok; final_cwd={}, out_dir={}",
            final_cwd.display(),
            out_dir.display()
        );
    }

    #[allow(clippy::disallowed_types)]
    let final_cwd_external = UnguardedPath::external(final_cwd.as_path().to_path_buf());

    let meta = resolver
        .metadata_unguarded(&final_cwd_external)
        .map_err(|e| {
            syn::Error::new(
                span,
                format!(
                    "final workdir missing after build: {} ({e})",
                    final_cwd.display()
                ),
            )
        })?;
    if !meta.is_dir() {
        return Err(syn::Error::new(
            span,
            format!("final workdir is not a directory: {}", final_cwd.display()),
        ));
    }

    // Ensure destination exists, then atomically stage-and-sync the new output
    // over the live directory (no destructive wipe window).
    ensure_out_dir(&resolver, out_dir, span)?;
    stage_materialize(&resolver, &final_cwd_external, out_dir)
        .map_err(|e| syn::Error::new(span, format!("failed to materialize into out_dir: {e:#}")))?;

    Ok(final_cwd)
}

fn ensure_out_dir(
    resolver: &PathResolver,
    out_dir: &GuardedPath,
    span: proc_macro2::Span,
) -> syn::Result<()> {
    if out_dir.as_path().exists() {
        if !out_dir.as_path().is_dir() {
            return Err(syn::Error::new(
                span,
                format!(
                    "out_dir exists but is not a directory: {}",
                    out_dir.display()
                ),
            ));
        }
        return Ok(());
    }
    resolver.create_dir_all(out_dir).map_err(|e| {
        syn::Error::new(
            span,
            format!("failed to create out_dir {}: {e}", out_dir.display()),
        )
    })
}

// ---------------------------------------------------------------------------
// oxdock! — runtime AST construction macro with #var interpolation
// ---------------------------------------------------------------------------

use oxdock_parser::{Arg, Expr, Step, StepKind, Value};

const INJECT_PREFIX: &str = "__OXDOCK_INJECT_";
const INJECT_SUFFIX: &str = "__";

fn placeholder_for(idx: usize) -> String {
    format!("{INJECT_PREFIX}{idx}{INJECT_SUFFIX}")
}

fn is_placeholder(s: &str) -> Option<usize> {
    s.strip_prefix(INJECT_PREFIX)
        .and_then(|rest| rest.strip_suffix(INJECT_SUFFIX))
        .and_then(|num| num.parse::<usize>().ok())
}

/// Runtime AST construction macro with `#var` interpolation.
///
/// Accepts the same DSL syntax as `oxdock_embed!`'s `script:` block,
/// but returns `Vec<oxdock_parser::Step>` instead of embedding files.
///
/// Use `#var` to inject Rust variables (must implement `Display`/`ToString`).
/// DSL variables (`$var` in LET/FOR) are distinct and unaffected.
#[proc_macro]
pub fn oxdock(input: TokenStream) -> TokenStream {
    match expand_oxdock(input) {
        Ok(ts) => ts,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_oxdock(input: TokenStream) -> syn::Result<TokenStream> {
    let ts: proc_macro2::TokenStream = input.into();
    let interp = collect_hash_idents(ts.clone())?;

    // Preprocess: remove `#` tokens and replace ident tokens with placeholders
    // so script_from_braced_tokens never sees the `#` sigil.
    let sanitized = sanitize_hash_tokens(ts, &interp)?;

    let dsl_text = oxdock_parser::script_from_braced_tokens(&sanitized).map_err(|e| {
        syn::Error::new(proc_macro2::Span::call_site(), format!("parse error: {e}"))
    })?;

    let dsl_text = dsl_text.trim();
    if dsl_text.is_empty() {
        let ts_out = quote! { Vec::<oxdock_parser::Step>::new() };
        return Ok(ts_out.into());
    }

    let steps = oxdock_parser::parse_script(dsl_text).map_err(|e| {
        let msg = format!("parse error: {e}\ndsl:\n{dsl_text}");
        syn::Error::new(proc_macro2::Span::call_site(), msg)
    })?;

    let step_tokens: Vec<_> = steps.iter().map(|step| emit_step(step, &interp)).collect();

    let ts_out = quote! {
        {
            use oxdock_parser::{Arg, Expr, Step, StepKind, Value,
                IoBinding, IoStream, WorkspaceTarget, GuardExpr, Guard, PlatformGuard};
            vec![#(#step_tokens),*]
        }
    };
    Ok(ts_out.into())
}

/// Walk the token stream and collect all `#ident` pairs, recursing into groups.
fn collect_hash_idents(
    ts: proc_macro2::TokenStream,
) -> syn::Result<Vec<(proc_macro2::Ident, usize)>> {
    let tokens: Vec<TokenTree> = ts.into_iter().collect();
    let mut interp: Vec<(proc_macro2::Ident, usize)> = Vec::new();
    let mut idx = 0;
    collect_hash_idents_inner(&tokens, &mut interp, &mut idx);
    Ok(interp)
}

fn collect_hash_idents_inner(
    tokens: &[TokenTree],
    interp: &mut Vec<(proc_macro2::Ident, usize)>,
    idx: &mut usize,
) {
    let mut i = 0;
    while i < tokens.len() {
        #[allow(clippy::collapsible_if)]
        if let TokenTree::Punct(p) = &tokens[i] {
            if p.as_char() == '#'
                && i + 1 < tokens.len()
                && let TokenTree::Ident(ident) = &tokens[i + 1]
            {
                if !interp.iter().any(|(id, _)| id == ident) {
                    interp.push((ident.clone(), *idx));
                    *idx += 1;
                }
                i += 2;
                continue;
            }
        }
        if let TokenTree::Group(g) = &tokens[i] {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            collect_hash_idents_inner(&inner, interp, idx);
        }
        i += 1;
    }
}

/// Remove `#` tokens and replace following ident tokens with placeholder idents,
/// recursing into groups so nested #var references are handled.
fn sanitize_hash_tokens(
    ts: proc_macro2::TokenStream,
    interp: &[(proc_macro2::Ident, usize)],
) -> syn::Result<proc_macro2::TokenStream> {
    let tokens: Vec<TokenTree> = ts.into_iter().collect();
    Ok(sanitize_hash_tokens_inner(&tokens, interp))
}

fn sanitize_hash_tokens_inner(
    tokens: &[TokenTree],
    interp: &[(proc_macro2::Ident, usize)],
) -> proc_macro2::TokenStream {
    let mut output = proc_macro2::TokenStream::new();
    let mut i = 0;
    while i < tokens.len() {
        #[allow(clippy::collapsible_if)]
        if let TokenTree::Punct(p) = &tokens[i] {
            if p.as_char() == '#'
                && i + 1 < tokens.len()
                && let TokenTree::Ident(ident) = &tokens[i + 1]
                && let Some((_, idx)) = interp.iter().find(|(id, _)| id == ident)
            {
                let ph = placeholder_for(*idx);
                output.extend(std::iter::once(TokenTree::Ident(proc_macro2::Ident::new(
                    &ph,
                    ident.span(),
                ))));
                i += 2;
                continue;
            }
        }
        if let TokenTree::Group(g) = &tokens[i] {
            let inner =
                sanitize_hash_tokens_inner(&g.stream().into_iter().collect::<Vec<_>>(), interp);
            let mut new_group = proc_macro2::Group::new(g.delimiter(), inner);
            new_group.set_span(g.span());
            output.extend(std::iter::once(TokenTree::Group(new_group)));
            i += 1;
            continue;
        }
        output.extend(std::iter::once(tokens[i].clone()));
        i += 1;
    }
    output
}

// ---------------------------------------------------------------------------
// AST emission helpers
// ---------------------------------------------------------------------------

fn emit_arg(arg: &Arg, interp: &[(proc_macro2::Ident, usize)]) -> proc_macro2::TokenStream {
    match arg {
        Arg::String(s) => {
            if let Some(idx) = is_placeholder(s) {
                let ident = &interp.iter().find(|(_, i)| *i == idx).unwrap().0;
                quote! { Arg::String(#ident.to_string()) }
            } else {
                quote! { Arg::String(#s.to_string()) }
            }
        }
        Arg::Expr(e) => {
            let tok = emit_expr(e, interp);
            quote! { Arg::Expr(#tok) }
        }
    }
}

fn emit_expr(expr: &Expr, interp: &[(proc_macro2::Ident, usize)]) -> proc_macro2::TokenStream {
    match expr {
        Expr::Literal(v) => emit_value(v, interp),
        Expr::Var(s) => {
            if let Some(idx) = is_placeholder(s) {
                let ident = &interp.iter().find(|(_, i)| *i == idx).unwrap().0;
                quote! { Expr::Var(#ident.to_string()) }
            } else {
                quote! { Expr::Var(#s.to_string()) }
            }
        }
        Expr::KeyPath { base, keys } => {
            let base_stream = if let Some(idx) = is_placeholder(base) {
                let ident = &interp.iter().find(|(_, i)| *i == idx).unwrap().0;
                quote! { #ident.to_string() }
            } else {
                quote! { #base.to_string() }
            };
            let key_strs: Vec<_> = keys
                .iter()
                .map(|k| {
                    if let Some(idx) = is_placeholder(k) {
                        let ident = &interp.iter().find(|(_, i)| *i == idx).unwrap().0;
                        quote! { #ident.to_string() }
                    } else {
                        quote! { #k.to_string() }
                    }
                })
                .collect();
            quote! { Expr::KeyPath { base: #base_stream, keys: vec![#(#key_strs),*] } }
        }
        Expr::List(items) => {
            let item_tokens: Vec<_> = items.iter().map(|e| emit_expr(e, interp)).collect();
            quote! { Expr::List(vec![#(#item_tokens),*]) }
        }
        Expr::Call { name, args } => {
            let arg_tokens: Vec<_> = args.iter().map(|e| emit_expr(e, interp)).collect();
            quote! { Expr::Call { name: #name.to_string(), args: vec![#(#arg_tokens),*] } }
        }
        Expr::Compare { op, left, right } => {
            let op_token = match op {
                oxdock_parser::ast::CompareOp::Eq => quote! { oxdock_parser::ast::CompareOp::Eq },
                oxdock_parser::ast::CompareOp::Ne => quote! { oxdock_parser::ast::CompareOp::Ne },
            };
            let left_tokens = emit_expr(left, interp);
            let right_tokens = emit_expr(right, interp);
            quote! {
                oxdock_parser::ast::Expr::Compare {
                    op: #op_token,
                    left: Box::new(#left_tokens),
                    right: Box::new(#right_tokens),
                }
            }
        }
        Expr::Logical { op, left, right } => {
            let op_token = match op {
                oxdock_parser::ast::LogicalOp::And => quote! { oxdock_parser::ast::LogicalOp::And },
                oxdock_parser::ast::LogicalOp::Or => quote! { oxdock_parser::ast::LogicalOp::Or },
            };
            let left_tokens = emit_expr(left, interp);
            let right_tokens = emit_expr(right, interp);
            quote! {
                oxdock_parser::ast::Expr::Logical {
                    op: #op_token,
                    left: Box::new(#left_tokens),
                    right: Box::new(#right_tokens),
                }
            }
        }
    }
}

fn emit_value(v: &Value, interp: &[(proc_macro2::Ident, usize)]) -> proc_macro2::TokenStream {
    let raw = emit_raw_value(v, interp);
    quote! { Expr::Literal(#raw) }
}

fn emit_raw_value(v: &Value, interp: &[(proc_macro2::Ident, usize)]) -> proc_macro2::TokenStream {
    match v {
        Value::String(s) => {
            if let Some(idx) = is_placeholder(s) {
                let ident = &interp.iter().find(|(_, i)| *i == idx).unwrap().0;
                quote! { Value::String(#ident.to_string()) }
            } else {
                quote! { Value::String(#s.to_string()) }
            }
        }
        Value::List(items) => {
            let item_tokens: Vec<_> = items
                .iter()
                .map(|item| emit_raw_value(item, interp))
                .collect();
            quote! { Value::List(vec![#(#item_tokens),*]) }
        }
        Value::Map(map) => {
            let mut pairs: Vec<_> = map.iter().collect();
            pairs.sort_by_key(|(k, _)| k.to_owned());
            let kv_tokens: Vec<_> = pairs
                .iter()
                .map(|(k, v)| {
                    let val_tokens = emit_raw_value(v, interp);
                    quote! { (#k.to_string(), #val_tokens) }
                })
                .collect();
            quote! { Value::Map(vec![#(#kv_tokens),*].into_iter().collect()) }
        }
        Value::Bool(b) => quote! { Value::Bool(#b) },
        Value::Int(i) => quote! { Value::Int(#i) },
    }
}

fn emit_stepkind(
    kind: &StepKind,
    interp: &[(proc_macro2::Ident, usize)],
) -> proc_macro2::TokenStream {
    match kind {
        StepKind::Workdir(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::Workdir(#t) }
        }
        StepKind::Run(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::Run(#t) }
        }
        StepKind::Echo(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::Echo(#t) }
        }
        StepKind::RunBg(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::RunBg(#t) }
        }
        StepKind::Mkdir(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::Mkdir(#t) }
        }
        StepKind::Cwd => quote! { StepKind::Cwd },
        StepKind::AssertDir(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::AssertDir(#t) }
        }
        StepKind::AssertAbsent(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::AssertAbsent(#t) }
        }
        StepKind::AssertStdout(a) => {
            let t = emit_arg(a, interp);
            quote! { StepKind::AssertStdout(#t) }
        }
        StepKind::Ls(opt) => match opt {
            Some(a) => {
                let t = emit_arg(a, interp);
                quote! { StepKind::Ls(Some(#t)) }
            }
            None => quote! { StepKind::Ls(None) },
        },
        StepKind::Read(opt) => match opt {
            Some(a) => {
                let t = emit_arg(a, interp);
                quote! { StepKind::Read(Some(#t)) }
            }
            None => quote! { StepKind::Read(None) },
        },
        StepKind::Env { key, value } => {
            let v = emit_arg(value, interp);
            quote! { StepKind::Env { key: #key.to_string(), value: #v } }
        }
        StepKind::Workspace(t) => {
            let variant = match t {
                oxdock_parser::WorkspaceTarget::Snapshot => quote! { Snapshot },
                oxdock_parser::WorkspaceTarget::Local => quote! { Local },
            };
            quote! { StepKind::Workspace(oxdock_parser::WorkspaceTarget::#variant) }
        }
        StepKind::InheritEnv { keys } => {
            quote! { StepKind::InheritEnv { keys: vec![#(#keys.to_string()),*] } }
        }
        StepKind::Copy {
            from_current_workspace,
            from,
            to,
        } => {
            let f = emit_arg(from, interp);
            let t = emit_arg(to, interp);
            quote! { StepKind::Copy { from_current_workspace: #from_current_workspace, from: #f, to: #t } }
        }
        StepKind::Symlink { from, to } => {
            let f = emit_arg(from, interp);
            let t = emit_arg(to, interp);
            quote! { StepKind::Symlink { from: #f, to: #t } }
        }
        StepKind::Write { path, contents } => {
            let p = emit_arg(path, interp);
            match contents {
                Some(a) => {
                    let c = emit_arg(a, interp);
                    quote! { StepKind::Write { path: #p, contents: Some(#c) } }
                }
                None => quote! { StepKind::Write { path: #p, contents: None } },
            }
        }
        StepKind::Append { path, contents } => {
            let p = emit_arg(path, interp);
            match contents {
                Some(a) => {
                    let c = emit_arg(a, interp);
                    quote! { StepKind::Append { path: #p, contents: Some(#c) } }
                }
                None => quote! { StepKind::Append { path: #p, contents: None } },
            }
        }
        StepKind::Expand { path, overrides } => match path {
            Some(a) => {
                let p = emit_arg(a, interp);
                let over: Vec<_> = overrides
                    .iter()
                    .map(|(k, v)| {
                        let vt = emit_arg(v, interp);
                        quote! { (#k.to_string(), #vt) }
                    })
                    .collect();
                quote! { StepKind::Expand { path: Some(#p), overrides: vec![#(#over),*] } }
            }
            None => {
                let over: Vec<_> = overrides
                    .iter()
                    .map(|(k, v)| {
                        let vt = emit_arg(v, interp);
                        quote! { (#k.to_string(), #vt) }
                    })
                    .collect();
                quote! { StepKind::Expand { path: None, overrides: vec![#(#over),*] } }
            }
        },
        StepKind::AssertFile {
            hash,
            path,
            contents,
        } => {
            let p = emit_arg(path, interp);
            match (hash, contents) {
                (Some(h), None) => {
                    quote! { StepKind::AssertFile { hash: Some(#h.to_string()), path: #p, contents: None } }
                }
                (None, Some(a)) => {
                    let c = emit_arg(a, interp);
                    quote! { StepKind::AssertFile { hash: None, path: #p, contents: Some(#c) } }
                }
                (None, None) => {
                    quote! { StepKind::AssertFile { hash: None, path: #p, contents: None } }
                }
                (Some(_), Some(_)) => {
                    unreachable!("grammar guarantees hash and contents are mutually exclusive")
                }
            }
        }
        StepKind::WithIo { bindings, cmd } => {
            let bindings_tokens = emit_io_bindings(bindings);
            let cmd_token = emit_stepkind(cmd, interp);
            quote! { StepKind::WithIo { bindings: #bindings_tokens, cmd: Box::new(#cmd_token) } }
        }
        StepKind::WithIoBlock { bindings } => {
            let bindings_tokens = emit_io_bindings(bindings);
            quote! { StepKind::WithIoBlock { bindings: #bindings_tokens } }
        }
        StepKind::CopyGit {
            rev,
            from,
            to,
            include_dirty,
        } => {
            let r = emit_arg(rev, interp);
            let f = emit_arg(from, interp);
            let t = emit_arg(to, interp);
            quote! { StepKind::CopyGit { rev: #r, from: #f, to: #t, include_dirty: #include_dirty } }
        }
        StepKind::HashSha256 { path } => {
            let p = emit_arg(path, interp);
            quote! { StepKind::HashSha256 { path: #p } }
        }
        StepKind::Exit(code) => quote! { StepKind::Exit(#code) },
        StepKind::For { key_var, var, in_expr, body } => {
            let in_tok = emit_expr(in_expr, interp);
            let body_tokens: Vec<_> = body.iter().map(|s| emit_step(s, interp)).collect();
            let key_var_tokens = match key_var {
                Some(k) => quote! { Some(#k.to_string()) },
                None => quote! { None },
            };
            quote! { StepKind::For { key_var: #key_var_tokens, var: #var.to_string(), in_expr: #in_tok, body: vec![#(#body_tokens),*] } }
        }
        StepKind::If {
            cond,
            then_body,
            else_ifs,
            else_body,
        } => {
            let cond_tokens = emit_expr(cond, interp);
            let then_tokens: Vec<_> = then_body.iter().map(|s| emit_step(s, interp)).collect();
            let else_if_tokens: Vec<_> = else_ifs
                .iter()
                .map(|(c, b)| {
                    let ec = emit_expr(c, interp);
                    let eb: Vec<_> = b.iter().map(|s| emit_step(s, interp)).collect();
                    quote! { (Box::new(#ec), vec![#(#eb),*]) }
                })
                .collect();
            let else_tokens = match else_body {
                Some(body) => {
                    let eb: Vec<_> = body.iter().map(|s| emit_step(s, interp)).collect();
                    quote! { Some(vec![#(#eb),*]) }
                }
                None => quote! { None },
            };
            quote! {
                oxdock_parser::ast::StepKind::If {
                    cond: Box::new(#cond_tokens),
                    then_body: vec![#(#then_tokens),*],
                    else_ifs: vec![#(#else_if_tokens),*],
                    else_body: #else_tokens,
                }
            }
        }
        StepKind::Assign { var, expr } => {
            let e = emit_expr(expr, interp);
            quote! { StepKind::Assign { var: #var.to_string(), expr: #e } }
        }
    }
}

fn emit_io_bindings(bindings: &[oxdock_parser::IoBinding]) -> proc_macro2::TokenStream {
    let tokens: Vec<_> = bindings
        .iter()
        .map(|b| {
            let stream = match b.stream {
                oxdock_parser::IoStream::Stdin => quote! { Stdin },
                oxdock_parser::IoStream::Stdout => quote! { Stdout },
                oxdock_parser::IoStream::Stderr => quote! { Stderr },
            };
            let pipe = b.pipe.as_ref().map(|p| quote! { Some(#p.to_string()) });
            quote! { oxdock_parser::IoBinding { stream: oxdock_parser::IoStream::#stream, pipe: #pipe } }
        })
        .collect();
    quote! { vec![#(#tokens),*] }
}

fn emit_guard(
    guard: &oxdock_parser::GuardExpr,
    interp: &[(proc_macro2::Ident, usize)],
) -> proc_macro2::TokenStream {
    match guard {
        oxdock_parser::GuardExpr::Predicate(g) => {
            let inner = emit_guard_pred(g, interp);
            quote! { oxdock_parser::GuardExpr::Predicate(#inner) }
        }
        oxdock_parser::GuardExpr::All(exprs) => {
            let inner: Vec<_> = exprs.iter().map(|e| emit_guard(e, interp)).collect();
            quote! { oxdock_parser::GuardExpr::All(vec![#(#inner),*]) }
        }
        oxdock_parser::GuardExpr::Or(exprs) => {
            let inner: Vec<_> = exprs.iter().map(|e| emit_guard(e, interp)).collect();
            quote! { oxdock_parser::GuardExpr::Or(vec![#(#inner),*]) }
        }
        oxdock_parser::GuardExpr::Not(inner) => {
            let inner_tok = emit_guard(inner, interp);
            quote! { oxdock_parser::GuardExpr::Not(Box::new(#inner_tok)) }
        }
    }
}

fn emit_guard_pred(
    g: &oxdock_parser::Guard,
    interp: &[(proc_macro2::Ident, usize)],
) -> proc_macro2::TokenStream {
    match g {
        oxdock_parser::Guard::Platform { target, invert } => {
            let target_variant = match target {
                oxdock_parser::PlatformGuard::Unix => quote! { Unix },
                oxdock_parser::PlatformGuard::Windows => quote! { Windows },
                oxdock_parser::PlatformGuard::Macos => quote! { Macos },
                oxdock_parser::PlatformGuard::Linux => quote! { Linux },
            };
            quote! { oxdock_parser::Guard::Platform { target: oxdock_parser::PlatformGuard::#target_variant, invert: #invert } }
        }
        oxdock_parser::Guard::EnvExists { key, invert } => {
            let k = resolve_placeholder_or_literal(key, interp);
            quote! { oxdock_parser::Guard::EnvExists { key: #k, invert: #invert } }
        }
        oxdock_parser::Guard::EnvEquals { key, value, invert } => {
            let k = resolve_placeholder_or_literal(key, interp);
            let v = resolve_placeholder_or_literal(value, interp);
            quote! { oxdock_parser::Guard::EnvEquals { key: #k, value: #v, invert: #invert } }
        }
        oxdock_parser::Guard::StaticBool { value, invert } => {
            let v = resolve_placeholder_or_literal(value, interp);
            quote! { oxdock_parser::Guard::StaticBool { value: #v, invert: #invert } }
        }
    }
}

fn resolve_placeholder_or_literal(
    s: &str,
    interp: &[(proc_macro2::Ident, usize)],
) -> proc_macro2::TokenStream {
    if let Some(idx) = is_placeholder(s) {
        let ident = &interp
            .iter()
            .find(|(_, i)| *i == idx)
            .expect("unmapped placeholder index in guard predicate")
            .0;
        quote! { #ident.to_string() }
    } else {
        quote! { #s.to_string() }
    }
}

fn emit_step(step: &Step, interp: &[(proc_macro2::Ident, usize)]) -> proc_macro2::TokenStream {
    let guard = match &step.guard {
        Some(g) => {
            let g_tok = emit_guard(g, interp);
            quote! { Some(#g_tok) }
        }
        None => quote! { None },
    };
    let kind = emit_stepkind(&step.kind, interp);
    let scope_enter = step.scope_enter;
    let scope_exit = step.scope_exit;
    quote! {
        oxdock_parser::Step {
            guard: #guard,
            kind: #kind,
            scope_enter: #scope_enter,
            scope_exit: #scope_exit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> (oxdock_fs::GuardedTempDir, PathResolver, GuardedPath) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        (temp, resolver, root)
    }

    #[test]
    fn force_flag_overrides_cache_validity() {
        assert!(
            should_rebuild(true, true),
            "force must bypass a valid cache"
        );
        assert!(!should_rebuild(false, true));
        assert!(should_rebuild(false, false));
        assert!(should_rebuild(true, false));
    }

    #[test]
    fn internal_artifacts_are_excluded_from_assets() {
        assert!(is_internal_artifact(".oxdock_hash"));
        assert!(is_internal_artifact(".oxdock-staging"));
        assert!(is_internal_artifact(".oxdock-staging/x"));
        assert!(is_internal_artifact(".oxdock-tempdir"));
        assert!(!is_internal_artifact("hello.txt"));
        assert!(!is_internal_artifact("nested/dir/file.bin"));
    }

    fn include_bytes_paths(ts: &proc_macro2::TokenStream) -> Vec<String> {
        use syn::visit::Visit;
        let file: syn::File = syn::parse2(ts.clone()).expect("parse output as file");

        struct IncludeVisitor {
            matches: Vec<String>,
        }

        impl<'ast> Visit<'ast> for IncludeVisitor {
            fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                if mac
                    .path
                    .segments
                    .last()
                    .map(|seg| seg.ident == "include_bytes")
                    .unwrap_or(false)
                    && let Ok(lit) = syn::parse2::<syn::LitStr>(mac.tokens.clone())
                {
                    self.matches.push(lit.value());
                }
                syn::visit::visit_macro(self, mac);
            }
        }

        let mut visitor = IncludeVisitor {
            matches: Vec::new(),
        };
        visitor.visit_file(&file);
        visitor.matches
    }

    #[test]
    fn join_guard_appends_relative_paths() {
        let temp = GuardedPath::tempdir().unwrap();
        let base = temp.as_guarded_path().clone();
        let joined = join_guard(&base, "some/rel/path", proc_macro2::Span::call_site()).unwrap();
        assert!(joined.as_path().starts_with(base.as_path()));
    }

    #[test]
    fn join_guard_rejects_paths_escaping_manifest_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let base = temp.as_guarded_path().clone();
        let err = join_guard(&base, "../outside", proc_macro2::Span::call_site());
        assert!(err.is_err(), "escaping paths must be rejected");
    }

    #[test]
    fn join_guard_accepts_within_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let base = temp.as_guarded_path().clone();
        let ok = join_guard(&base, "./inside", proc_macro2::Span::call_site());
        assert!(ok.is_ok());
    }

    #[test]
    fn uses_final_workdir_for_folder() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let manifest_dir = temp.as_guarded_path().clone();
        let resolver =
            PathResolver::new_guarded(manifest_dir.clone(), manifest_dir.clone()).unwrap();
        resolver
            .write_file(&manifest_dir.join("seed.txt").unwrap(), b"seed")
            .ok();

        // Route CARGO_MANIFEST_DIR at the fixture root for this test.
        let _guard = oxdock_sys_test_utils::TestEnvGuard::set(
            "CARGO_MANIFEST_DIR",
            manifest_dir.display().to_string().as_str(),
        );
        let _ws = oxdock_sys_test_utils::TestEnvGuard::remove("OXDOCK_WORKSPACE_ROOT");

        let assets_rel = "prebuilt";
        let script = ["MKDIR dist", "WRITE dist/hello.txt hi", "WORKDIR dist"].join("\n");

        let input = DslMacroInput {
            name: syn::Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(syn::LitStr::new(
                &script,
                proc_macro2::Span::call_site(),
            )),
            out_dir: syn::LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let ts = expand_embed_internal(&input).expect("inline build should succeed");
        let include_paths = include_bytes_paths(&ts);
        assert_eq!(include_paths.len(), 1, "only final WORKDIR file embedded");
        assert!(
            include_paths[0].contains("prebuilt") && include_paths[0].contains("hello.txt"),
            "unexpected path: {:?}",
            include_paths[0]
        );
    }

    #[test]
    fn cache_validation_requires_matching_hash_and_directory() {
        let (_temp, resolver, root) = make_ctx();
        let out = root.join("prebuilt").expect("join");
        resolver.create_dir_all(&out).expect("mkdir");

        let plan = InlinePlan {
            script_src: "WRITE x.txt y".into(),
            script_span: proc_macro2::Span::call_site(),
            manifest_resolver: resolver,
            out_dir: out,
            fingerprint: "deadbeef".into(),
        };
        assert!(!cached_out_dir_valid(&plan), "no hash file yet");

        let hash_path = plan.out_dir.join(HASH_FILE).unwrap();
        plan.manifest_resolver
            .write_file(&hash_path, b"cafebabe\n")
            .unwrap();
        assert!(!cached_out_dir_valid(&plan), "mismatched hash");

        plan.manifest_resolver
            .write_file(&hash_path, b" deadbeef \n")
            .unwrap();
        assert!(cached_out_dir_valid(&plan), "trimmed equality");
    }
}
