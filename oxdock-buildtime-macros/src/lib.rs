//! OxDock's primary user-facing contract: declarative build scripts written
//! inline, directly alongside Rust code.
//!
//! ```rust,ignore
//! use oxdock_buildtime_macros::embed;
//!
//! embed! {
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
//! any drift rebuilds. `prepare!` behaves identically but emits no runtime
//! module.

use oxdock_buildtime_helpers::{
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
use quote::quote;
use syn::parse_macro_input;

/// Runs the DSL at compile-time, materializes assets, and emits a lightweight
/// struct with embedded bytes pointing at the output directory.
///
/// See the crate docs for the full inline contract.
#[proc_macro]
pub fn embed(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DslMacroInput);
    expand_embed_tokens(&input).into()
}

/// Executes the DSL like [`embed`] but emits no runtime module. Use this when
/// the assets only need to exist during the build.
#[proc_macro]
pub fn prepare(input: TokenStream) -> TokenStream {
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
