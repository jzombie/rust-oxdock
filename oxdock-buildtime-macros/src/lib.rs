use oxdock_embed::{emit_embed_module, gather_assets, runtime_support_tokens};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_parser::{DslMacroInput, ScriptSource};
use proc_macro::TokenStream;
use quote::quote;
use syn::parse_macro_input;

// TODO: Update example and don't ignore
/// Macro that runs the DSL at compile-time, materializes assets into a temp
/// dir, and emits a lightweight struct with embedded bytes pointing at that dir.
///
/// ```rust,ignore
/// use oxdock_buildtime_macros::embed;
///
/// embed! {
///     name: DemoAssets,
///     script: r#"...DSL..."#,
///     out_dir: "prebuilt",
/// }
/// ```
#[proc_macro]
pub fn embed(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DslMacroInput);
    expand_embed_tokens(&input).into()
}

/// Macro similar to `embed!` but only prepares (builds/copies) the
/// out_dir at compile time and emits no runtime struct. Use this when you
/// want the assets present on disk during build but don't want an embedded
/// struct generated into the consuming crate.
#[proc_macro]
pub fn prepare(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DslMacroInput);
    match expand_prepare_internal(&input) {
        Ok(()) => TokenStream::new(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_prepare_internal(input: &DslMacroInput) -> syn::Result<()> {
    match prepare_macro_plan(input)? {
        MacroPlan::Skip => {
            tracing::info!("prepare: skipping build due to embed skip flag");
            Ok(())
        }
        MacroPlan::Ready(plan) => {
            let PreparedMacroPlan {
                script_src,
                script_span,
                manifest_resolver: _,
                out_dir,
                out_dir_span,
                should_build,
                force_rebuild,
            } = *plan;

            if should_build {
                preflight_out_dir_for_build(&out_dir, out_dir_span)?;
                if force_rebuild {
                    tracing::info!(
                        "prepare: force rebuilding assets into {}",
                        out_dir.display()
                    );
                } else {
                    tracing::info!("prepare: rebuilding assets into {}", out_dir.display());
                }
                let _final_folder = build_assets(&script_src, script_span, &out_dir)?;
                return Ok(());
            }

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
                tracing::info!("prepare: reusing assets at {}", out_dir.display());
                return Ok(());
            }

            Err(syn::Error::new(
                script_span,
                format!(
                    "prepare: refused to build assets (not primary package or .git missing) and out_dir missing at {}",
                    out_dir.display()
                ),
            ))
        }
    }
}
fn join_guard(base: &GuardedPath, rel: &str, span: proc_macro2::Span) -> syn::Result<GuardedPath> {
    base.join(rel)
        .map_err(|e| syn::Error::new(span, e.to_string()))
}

/// Produce a normalized literal path by reusing the shared path normalizer in
/// oxdock-fs (it strips Windows verbatim prefixes).
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

fn expand_embed_internal(input: &DslMacroInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.name;

    match prepare_macro_plan(input)? {
        MacroPlan::Skip => {
            tracing::info!("embed: skipping build due to embed skip flag");
            Ok(embed_error_stub(name))
        }
        MacroPlan::Ready(plan) => {
            let PreparedMacroPlan {
                script_src,
                script_span,
                manifest_resolver,
                out_dir,
                out_dir_span,
                should_build,
                force_rebuild,
            } = *plan;

            if should_build {
                preflight_out_dir_for_build(&out_dir, out_dir_span)?;
                if force_rebuild {
                    tracing::info!("embed: force rebuilding assets into {}", out_dir.display());
                } else {
                    tracing::info!("embed: rebuilding assets into {}", out_dir.display());
                }
                let _final_folder = build_assets(&script_src, script_span, &out_dir)?;
                let assets = gather_assets(&manifest_resolver, &out_dir)
                    .map_err(|e| syn::Error::new(script_span, e.to_string()))?;
                return emit_embed_module(name, &assets);
            }

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
                tracing::info!("embed: reusing assets at {}", out_dir.display());
                let assets = gather_assets(&manifest_resolver, &out_dir)
                    .map_err(|e| syn::Error::new(script_span, e.to_string()))?;
                return emit_embed_module(name, &assets);
            }

            Err(syn::Error::new(
                script_span,
                format!(
                    "embed: refused to build assets (not primary package or .git missing) and out_dir missing at {}",
                    out_dir.display()
                ),
            ))
        }
    }
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
    let debug_embed = embed_debug_enabled_from(std::env::var("OXDOCK_EMBED_DEBUG").ok());

    // Build in a temp dir; only the final workdir gets materialized into out_dir.
    let tempdir = GuardedPath::tempdir()
        .map_err(|e| syn::Error::new(span, format!("failed to create temp dir: {e}")))?;
    let temp_root_guard = tempdir.as_guarded_path().clone();

    let steps = oxdock_parser::parse_script(script)
        .map_err(|e| syn::Error::new(span, format!("parse error: {e}")))?;

    let resolver =
        PathResolver::from_manifest_env().map_err(|e| syn::Error::new(span, e.to_string()))?;
    let workspace_root =
        oxdock_fs::discover_workspace_root().map_err(|e| syn::Error::new(span, e.to_string()))?;

    let final_cwd = oxdock_core::run_steps_with_context_result_with_io(
        &temp_root_guard,
        &workspace_root,
        &steps,
        oxdock_core::ExecIo::new(),
    )
    .map_err(|e| {
        // IMPORTANT: Use alternate formatting to include the full error chain and filesystem snapshot.
        syn::Error::new(span, format!("execution error: {e:#}"))
    })?;

    if debug_embed {
        eprintln!(
            "oxdock: build_assets script ok; final_cwd={}, out_dir={}",
            final_cwd.display(),
            out_dir.display()
        );
    }

    #[allow(clippy::disallowed_types)]
    let final_cwd_external = oxdock_fs::UnguardedPath::new(final_cwd.as_path().to_path_buf());

    tracing::info!(
        "embed: final workdir {} (temp root {})",
        final_cwd.display(),
        temp_root_guard.display()
    );

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

    // Clean destination then copy the final workdir contents into the out_dir mount.
    if out_dir.as_path().exists() {
        clear_dir(out_dir, span)?;
    } else {
        resolver.create_dir_all(out_dir).map_err(|e| {
            syn::Error::new(
                span,
                format!("failed to create out_dir {}: {e}", out_dir.display()),
            )
        })?;
    }

    resolver
        .copy_dir_from_unguarded(&final_cwd_external, out_dir)
        .map_err(|e| {
            syn::Error::new(
                span,
                format!("failed to copy final workdir into out_dir: {e}"),
            )
        })?;
    if debug_embed {
        eprintln!(
            "oxdock: build_assets copied into out_dir={}, entries={:?}",
            out_dir.display(),
            resolver.read_dir_entries(out_dir).ok().map(|v| v.len())
        );
    }
    tracing::info!(
        "embed: populated out_dir from final workdir; entries now: {}",
        count_entries(out_dir, span)?
    );

    Ok(final_cwd)
}

// `copy_dir_contents` replaced by `PathResolver::copy_dir_from_external`.

fn clear_dir(dir: &GuardedPath, span: proc_macro2::Span) -> syn::Result<()> {
    // Use PathResolver for deletions to keep filesystem access centralized.
    let resolver =
        PathResolver::from_manifest_env().map_err(|e| syn::Error::new(span, e.to_string()))?;

    // Validate dir is a directory (use std as this is already an existing path under manifest)
    if !dir.as_path().is_dir() {
        return Err(syn::Error::new(
            span,
            format!("out_dir exists but is not a directory: {}", dir.display()),
        ));
    }

    let entries = resolver.read_dir_entries(dir).map_err(|e| {
        syn::Error::new(
            span,
            format!("failed to read out_dir {}: {e}", dir.display()),
        )
    })?;

    for entry in entries {
        let path = entry.path();
        let guarded = GuardedPath::new(dir.root(), &path).map_err(|e| {
            syn::Error::new(
                span,
                format!("failed to guard entry {}: {e}", path.display()),
            )
        })?;
        let ft = entry
            .file_type()
            .map_err(|e| syn::Error::new(span, format!("file type error: {e}")))?;
        if ft.is_dir() {
            resolver.remove_dir_all(&guarded).map_err(|e| {
                syn::Error::new(
                    span,
                    format!("failed to remove dir {}: {e}", path.display()),
                )
            })?;
        } else {
            resolver.remove_file(&guarded).map_err(|e| {
                syn::Error::new(
                    span,
                    format!("failed to remove file {}: {e}", path.display()),
                )
            })?;
        }
    }
    Ok(())
}

fn count_entries(dir: &GuardedPath, span: proc_macro2::Span) -> syn::Result<usize> {
    let resolver =
        PathResolver::from_manifest_env().map_err(|e| syn::Error::new(span, e.to_string()))?;
    let entries = resolver
        .read_dir_entries(dir)
        .map_err(|e| syn::Error::new(span, format!("failed to read dir {}: {e}", dir.display())))?;
    Ok(entries.len())
}

/// Determines if the macro execution should be skipped to prevent heavy build operations
/// during IDE analysis.
///
/// This is used to prevent IDE warnings and performance issues resulting from
/// `rust-analyzer` (or other tools) executing the macro logic in a background process.
/// Since `embed!` and `prepare!` can involve significant work (script execution,
/// file I/O), running them during every keystroke analysis is undesirable.
fn embed_execution_is_skipped() -> bool {
    embed_execution_is_skipped_with(
        |key| std::env::var(key).ok(),
        || std::env::current_exe().ok(),
    )
}

/// Pure form of [`embed_execution_is_skipped`] with injectable lookups so the
/// branch matrix is unit-testable without mutating process-global state.
#[allow(clippy::disallowed_types)]
fn embed_execution_is_skipped_with(
    env: impl Fn(&str) -> Option<String>,
    current_exe: impl FnOnce() -> Option<std::path::PathBuf>,
) -> bool {
    // Runtime check: rust-analyzer sets this variable in the proc-macro server process.
    if env("RUST_ANALYZER_INTERNALS_DO_NOT_USE").is_some() {
        return true;
    }

    // Skip when running under a Miri-configured build (e.g., clippy with --cfg miri),
    // since proc-macro execution can touch filesystem APIs that Miri does not support.
    if env("RUSTFLAGS")
        .map(|flags| flags.contains("--cfg miri"))
        .unwrap_or(false)
    {
        return true;
    }

    // Fallback: Check executable name
    if current_exe()
        .map(|pb| pb.to_string_lossy().contains("rust-analyzer"))
        .unwrap_or(false)
    {
        return true;
    }

    // Fallback 2: VS Code background task detection
    // If we are running inside VS Code (detected via VSCODE_PID), but TERM is missing,
    // it is likely a background analysis task (like rust-analyzer running cargo check)
    // rather than a user-initiated terminal command.
    if env("VSCODE_PID").is_some() && env("TERM").is_none() {
        return true;
    }

    false
}

/// Truthiness rules for `OXDOCK_EMBED_DEBUG`: enabled by `1` or any casing of
/// `true`; everything else (including absence) is disabled.
fn embed_debug_enabled_from(value: Option<String>) -> bool {
    value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

enum MacroPlan {
    Skip,
    Ready(Box<PreparedMacroPlan>),
}

struct PreparedMacroPlan {
    script_src: String,
    script_span: proc_macro2::Span,
    manifest_resolver: PathResolver,
    out_dir: GuardedPath,
    out_dir_span: proc_macro2::Span,
    should_build: bool,
    force_rebuild: bool,
}

fn prepare_macro_plan(input: &DslMacroInput) -> syn::Result<MacroPlan> {
    let (script_src, script_span) = match &input.script {
        ScriptSource::Literal(lit) => (lit.value(), lit.span()),
        ScriptSource::Braced(ts) => (
            oxdock_parser::script_from_braced_tokens(ts).map_err(|e: anyhow::Error| {
                syn::Error::new(proc_macro2::Span::call_site(), e.to_string())
            })?,
            proc_macro2::Span::call_site(),
        ),
    };

    let manifest_resolver = PathResolver::from_manifest_env()
        .map_err(|e| syn::Error::new(script_span, e.to_string()))?;

    if embed_execution_is_skipped() {
        return Ok(MacroPlan::Skip);
    }

    let is_primary = std::env::var("CARGO_PRIMARY_PACKAGE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let has_git = manifest_resolver
        .has_git_dir()
        .map_err(|e| syn::Error::new(script_span, e.to_string()))?;
    let force_rebuild = std::env::var("OXDOCK_EMBED_FORCE_REBUILD")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let should_build = has_git || is_primary || force_rebuild;

    let manifest_root = manifest_resolver.root().clone();
    let out_dir_str = input.out_dir.value();
    let out_dir = join_guard(&manifest_root, &out_dir_str, input.out_dir.span())?;

    Ok(MacroPlan::Ready(Box::new(PreparedMacroPlan {
        script_src,
        script_span,
        manifest_resolver,
        out_dir,
        out_dir_span: input.out_dir.span(),
        should_build,
        force_rebuild,
    })))
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    #[allow(clippy::disallowed_types)]
    use oxdock_fs::{GuardedPath, UnguardedPath};
    use oxdock_parser::StepKind;
    use oxdock_process::serial_cargo_env::manifest_env_guard;
    use syn::{Ident, LitStr, visit::Visit};

    macro_rules! dsl_tokens {
        ($($tt:tt)*) => {{
            use quote::quote;
            let tokens: proc_macro2::TokenStream = quote! { $($tt)* };
            tokens
        }};
    }

    fn guard_root(path: &UnguardedPath) -> GuardedPath {
        GuardedPath::new_root(path.as_path()).unwrap()
    }

    fn resolver_for(root: &GuardedPath) -> PathResolver {
        PathResolver::new(root.as_path(), root.as_path()).unwrap()
    }

    #[test]
    fn errors_when_out_dir_is_file_before_build() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);
        // Create .git dir via PathResolver to centralize filesystem access.
        let resolver = resolver_for(&manifest_dir);
        resolver
            .create_dir_all(&manifest_dir.join(".git").unwrap())
            .expect("mkdir .git");

        let assets_rel = "prebuilt";
        let assets_abs = manifest_dir.join(assets_rel).unwrap();
        resolver
            .write_file(&assets_abs, b"not a dir")
            .expect("create file at out_dir path");

        let _env = manifest_env_guard(&manifest_dir, true);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new(
                "WRITE hello.txt hi",
                proc_macro2::Span::call_site(),
            )),
            out_dir: LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let err = expand_embed_internal(&input).expect_err("should fail when out_dir is file");
        let msg = err.to_string();
        assert!(
            msg.contains("out_dir exists but is not a directory"),
            "message should report non-directory out_dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_out_dir_not_writable_before_build() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);
        let resolver = resolver_for(&manifest_dir);
        resolver
            .create_dir_all(&manifest_dir.join(".git").unwrap())
            .expect("mkdir .git");

        let assets_rel = "prebuilt";
        let assets_abs = manifest_dir.join(assets_rel).unwrap();
        resolver.create_dir_all(&assets_abs).expect("mkdir out_dir");
        resolver
            .set_permissions_mode_unix(&assets_abs, 0o555)
            .expect("make out_dir read-only");

        let _env = manifest_env_guard(&manifest_dir, true);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new(
                "WRITE hello.txt hi",
                proc_macro2::Span::call_site(),
            )),
            out_dir: LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let err = expand_embed_internal(&input).expect_err("should fail when out_dir not writable");
        let msg = err.to_string();
        resolver
            .set_permissions_mode_unix(&assets_abs, 0o755)
            .expect("restore permissions for cleanup");
        assert!(
            msg.contains("out_dir not writable"),
            "message should report non-writable out_dir"
        );
    }

    #[test]
    fn embed_error_stub_contains_placeholder_api() {
        let name = Ident::new("DemoAssets", proc_macro2::Span::call_site());
        let stub = super::embed_error_stub(&name).to_string();
        assert!(
            stub.contains("mod __oxdock_embed_DemoAssets"),
            "stub should wrap struct in module: {stub}"
        );
        assert!(
            stub.contains("pub struct DemoAssets"),
            "stub should define requested struct: {stub}"
        );
        assert!(
            stub.contains("pub fn get"),
            "stub should expose get() method: {stub}"
        );
        assert!(
            stub.contains("Filenames :: from_slice"),
            "stub iter() should construct Filenames from slice: {stub}"
        );
    }

    #[test]
    fn embed_tokens_include_compile_error_and_stub_on_failure() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);

        let _env = manifest_env_guard(&manifest_dir, false);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new("", proc_macro2::Span::call_site())),
            out_dir: LitStr::new("missing", proc_macro2::Span::call_site()),
        };

        let tokens = super::expand_embed_tokens(&input);
        let output = tokens.to_string();
        assert!(
            output.contains("compile_error"),
            "tokens should include compile_error call: {output}"
        );
        assert!(
            output.contains("__oxdock_embed_DemoAssets"),
            "tokens should include stub module: {output}"
        );
    }

    #[test]
    fn embed_module_ident_prefixes_struct_name() {
        let name = Ident::new("DemoAssets", proc_macro2::Span::call_site());
        let module = super::embed_module_ident(&name);
        assert_eq!(module.to_string(), "__oxdock_embed_DemoAssets");
    }

    #[test]
    fn join_guard_appends_relative_paths() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let base = temp.as_guarded_path().clone();
        let joined =
            super::join_guard(&base, "nested/file.txt", proc_macro2::Span::call_site()).unwrap();
        assert!(
            joined.as_path().ends_with("nested/file.txt"),
            "join_guard should append relative paths"
        );
        assert_eq!(joined.root(), base.root(), "root should be preserved");
    }

    #[test]
    fn normalizes_braced_script() {
        let ts = dsl_tokens! {
            WORKDIR /
            MKDIR assets;
            WRITE assets/hello.txt "hi there";
            LS; LS; LS; RUN echo; LS;
            RUN echo && ls
        };

        let normalized =
            oxdock_parser::script_from_braced_tokens(&ts).expect("normalize braced script");
        let expected = [
            "WORKDIR /",
            "MKDIR assets;",
            "WRITE assets/hello.txt \"hi there\";",
            "LS;",
            "LS;",
            "LS;",
            "RUN echo;",
            "LS;",
            "RUN echo && ls",
        ]
        .join("\n");

        assert_eq!(normalized, expected);
    }

    #[test]
    fn braced_script_with_guard_block_parses() {
        let ts = dsl_tokens! {
            WORKDIR /
            MKDIR scoped
            MKDIR scoped/nested
            [env:TEST_SCOPE] {
                WORKDIR scoped
                WRITE inner.txt inside
                ENV SCOPE_FLAG=1
                [env:SCOPE_FLAG] {
                    WORKDIR nested
                    WRITE deep.txt nested
                    ENV INNER_ONLY=1
                }
                WRITE after_nested.txt still-scoped
                [env:INNER_ONLY] WRITE leaked_inner.txt nope
            }
            WRITE outside.txt outside
            [env:SCOPE_FLAG] WRITE leaked.txt nope
        };
        let steps = oxdock_parser::parse_braced_tokens(&ts).expect("braced script should parse");
        assert_eq!(steps.len(), 13, "expected 13 commands");
        assert_eq!(steps[3].scope_enter, 1, "outer block enter");
        assert_eq!(steps[10].scope_exit, 1, "outer block exit");
        assert_eq!(steps[6].scope_enter, 1, "nested block enter");
        assert_eq!(steps[8].scope_exit, 1, "nested block exit");
        match &steps[0].kind {
            StepKind::Workdir(path) => assert_eq!(path, "/"),
            other => panic!("expected WORKDIR /, saw {:?}", other),
        }
        match &steps[3].kind {
            StepKind::Workdir(path) => assert_eq!(path, "scoped"),
            other => panic!("expected scoped WORKDIR, saw {:?}", other),
        }
        match &steps[10].kind {
            StepKind::Write { path, .. } => assert_eq!(path, "leaked_inner.txt"),
            other => panic!("expected leaked inner WRITE, saw {:?}", other),
        }
        assert!(steps[10].guard.is_some(), "leaked_inner should be guarded");
        assert!(steps[12].guard.is_some(), "outer leak should be guarded");
    }

    #[test]
    fn uses_out_dir_when_not_primary_and_no_git() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);
        let assets_rel = "prebuilt";
        let assets_abs = manifest_dir.join(assets_rel).unwrap();
        let resolver = resolver_for(&manifest_dir);
        resolver.create_dir_all(&assets_abs).expect("mkdir out_dir");
        let sample_file = assets_abs.join("existing.txt").unwrap();
        resolver
            .write_file(&sample_file, b"prebuilt content")
            .expect("seed prebuilt file");

        // Simulate crates.io tarball: no .git, not primary package.
        let _env = manifest_env_guard(&manifest_dir, false);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new("", proc_macro2::Span::call_site())),
            out_dir: LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let ts = expand_embed_internal(&input).expect("out_dir branch should succeed");
        let out = ts.to_string();
        assert!(out.contains("DemoAssets"), "should define struct name");

        let include_paths = include_bytes_paths(&ts);
        assert_eq!(
            include_paths.len(),
            1,
            "preseeded out_dir should expose embedded paths"
        );
        assert_eq!(
            include_paths[0],
            oxdock_fs::normalized_path(&sample_file),
            "embed should reference files under out_dir"
        );
    }

    #[test]
    fn prepare_errors_without_out_dir_when_not_primary_and_no_git() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);

        let _env = manifest_env_guard(&manifest_dir, false);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new("", proc_macro2::Span::call_site())),
            out_dir: LitStr::new("missing", proc_macro2::Span::call_site()),
        };

        let err =
            expand_prepare_internal(&input).expect_err("prepare should require existing out_dir");
        let msg = err.to_string();
        assert!(
            msg.contains("prepare: refused to build assets") && msg.contains("missing"),
            "error should mention missing out_dir and refusal to build"
        );
    }

    #[test]
    fn errors_without_out_dir_when_not_primary_and_no_git() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);

        let _env = manifest_env_guard(&manifest_dir, false);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new("", proc_macro2::Span::call_site())),
            out_dir: LitStr::new("missing", proc_macro2::Span::call_site()),
        };

        let err = expand_embed_internal(&input).expect_err("should require out_dir path");
        let msg = err.to_string();
        assert!(
            msg.contains("out_dir missing"),
            "error should mention missing out_dir"
        );
    }

    #[test]
    fn builds_from_manifest_dir_when_primary_with_git() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);
        let resolver = resolver_for(&manifest_dir);
        resolver
            .create_dir_all(&manifest_dir.join(".git").unwrap())
            .expect("mkdir .git");
        let assets_rel = "prebuilt";
        let assets_abs = manifest_dir.join(assets_rel).unwrap();

        // Source file only exists under the provided manifest dir; COPY should succeed from there.
        resolver
            .write_file(
                &manifest_dir.join("source.txt").unwrap(),
                b"hello from manifest",
            )
            .expect("write source");

        let _env = manifest_env_guard(&manifest_dir, true);

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new(
                "COPY source.txt copied.txt",
                proc_macro2::Span::call_site(),
            )),
            out_dir: LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let ts = expand_embed_internal(&input).expect("should build using manifest dir");
        let copied_guard = assets_abs.join("copied.txt").unwrap();
        let include_paths = include_bytes_paths(&ts);
        assert!(
            include_paths
                .iter()
                .any(|p| p == &oxdock_fs::normalized_path(&copied_guard)),
            "embed should include copied.txt under out_dir"
        );
        let contents = resolver
            .read_to_string(&copied_guard)
            .expect("copied file readable");
        assert_eq!(
            contents, "hello from manifest",
            "copy should read from manifest dir"
        );
    }

    #[test]
    fn uses_final_workdir_for_folder() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let temp_root = UnguardedPath::new(temp.as_path());
        let manifest_dir = guard_root(&temp_root);
        let resolver = resolver_for(&manifest_dir);
        resolver
            .create_dir_all(&manifest_dir.join(".git").unwrap())
            .expect("mkdir .git");
        let assets_rel = "prebuilt";
        let assets_abs = manifest_dir.join(assets_rel).unwrap();

        let _env = manifest_env_guard(&manifest_dir, true);

        let script = [
            "MKDIR dist",
            "WRITE dist/hello.txt hi",
            "WRITE outside.txt nope",
            "WORKDIR dist",
        ]
        .join("\n");

        let input = DslMacroInput {
            name: Ident::new("DemoAssets", proc_macro2::Span::call_site()),
            script: ScriptSource::Literal(LitStr::new(&script, proc_macro2::Span::call_site())),
            out_dir: LitStr::new(assets_rel, proc_macro2::Span::call_site()),
        };

        let ts = expand_embed_internal(&input).expect("should build using final WORKDIR");
        let include_paths = include_bytes_paths(&ts);
        assert_eq!(
            include_paths.len(),
            1,
            "only final WORKDIR file should be embedded"
        );
        let asset_path = &include_paths[0];
        assert!(
            asset_path.ends_with(&format!("{assets_rel}/hello.txt")),
            "embedded file should live under out_dir"
        );

        let inside = assets_abs.join("hello.txt").expect("join hello.txt");
        assert!(
            inside.as_path().exists(),
            "file in final WORKDIR should exist in out_dir"
        );

        let outside = assets_abs.join("outside.txt").expect("join outside.txt");
        assert!(
            !outside.as_path().exists(),
            "only final WORKDIR contents should be copied into out_dir"
        );
    }

    fn include_bytes_paths(ts: &proc_macro2::TokenStream) -> Vec<String> {
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

    // ---------- hardening: escape rejection, skip predicates, IO edges ----------

    #[test]
    fn join_guard_rejects_paths_escaping_manifest_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let base = temp.as_guarded_path().clone();

        let err = join_guard(&base, "../outside.txt", proc_macro2::Span::call_site()).unwrap_err();
        assert!(
            err.to_string().contains("escapes"),
            "escape attempts must be rejected, got: {err}"
        );

        // Deep escapes through nested parents are caught identically.
        let deep = join_guard(
            &base,
            "a/b/../../../outside",
            proc_macro2::Span::call_site(),
        );
        assert!(deep.is_err());
    }

    #[test]
    fn join_guard_accepts_paths_within_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let base = temp.as_guarded_path().clone();

        let ok = join_guard(&base, "sub/dir/out", proc_macro2::Span::call_site())
            .expect("in-root path must be accepted");
        assert!(ok.as_path().starts_with(base.as_path()));

        // `..` that clamps back to the root itself is still contained.
        let clamp = join_guard(&base, "sub/..", proc_macro2::Span::call_site())
            .expect("clamped path stays inside");
        assert_eq!(clamp.as_path(), base.as_path());
    }

    #[test]
    fn embed_skip_predicate_covers_all_branches() {
        let clean = |_key: &str| None;
        let no_exe = || None;

        // Nothing set -> run normally.
        assert!(!super::embed_execution_is_skipped_with(clean, no_exe));

        // rust-analyzer runtime marker.
        let ra_marker =
            |key: &str| (key == "RUST_ANALYZER_INTERNALS_DO_NOT_USE").then(|| "1".to_string());
        assert!(super::embed_execution_is_skipped_with(ra_marker, no_exe));

        // Miri-configured RUSTFLAGS.
        let miri_flags =
            |key: &str| (key == "RUSTFLAGS").then(|| "--cfg miri -Zunstable-options".to_string());
        assert!(super::embed_execution_is_skipped_with(miri_flags, no_exe));

        let unrelated_flags = |key: &str| (key == "RUSTFLAGS").then(|| "-Dwarnings".to_string());
        assert!(!super::embed_execution_is_skipped_with(
            unrelated_flags,
            no_exe
        ));

        // Executable-name fallback.
        #[allow(clippy::disallowed_types)]
        let ra_exe = std::path::PathBuf::from("/tools/rust-analyzer-proc-macro-srv");
        assert!(super::embed_execution_is_skipped_with(clean, || Some(
            ra_exe
        )));
        #[allow(clippy::disallowed_types)]
        let cargo_exe = std::path::PathBuf::from("/bin/cargo");
        assert!(!super::embed_execution_is_skipped_with(clean, || Some(
            cargo_exe
        )));

        // VS Code background heuristic: VSCODE_PID without TERM.
        let vscode_bg = |key: &str| match key {
            "VSCODE_PID" => Some("4242".to_string()),
            _ => None,
        };
        assert!(super::embed_execution_is_skipped_with(vscode_bg, no_exe));

        let vscode_terminal = |key: &str| match key {
            "VSCODE_PID" => Some("4242".to_string()),
            "TERM" => Some("xterm-256color".to_string()),
            _ => None,
        };
        assert!(!super::embed_execution_is_skipped_with(
            vscode_terminal,
            no_exe
        ));
    }

    #[test]
    fn embed_debug_flag_truthiness_matrix() {
        for value in ["1", "true", "TRUE", "True"] {
            assert!(
                super::embed_debug_enabled_from(Some(value.to_string())),
                "{value} must enable debug"
            );
        }
        for value in ["0", "", "yes", "false"] {
            assert!(
                !super::embed_debug_enabled_from(Some(value.to_string())),
                "{value} must not enable debug"
            );
        }
        assert!(!super::embed_debug_enabled_from(None));
    }

    /// Runs `body` with a single tempdir serving as both the fake manifest
    /// root (for `from_manifest_env` lookups) and the target directory.
    fn in_manifest_scope<R>(body: impl FnOnce(&GuardedPath) -> R) -> R {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let _guard = manifest_env_guard(&root, true);
        body(&root)
    }

    #[cfg_attr(miri, ignore = "clear_dir inspects real host directories")]
    #[test]
    fn clear_dir_reports_non_directory_destination() {
        in_manifest_scope(|root| {
            let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
            let file = root.join("plain.txt").unwrap();
            resolver.write_file(&file, b"x").unwrap();

            let err = super::clear_dir(&file, proc_macro2::Span::call_site()).unwrap_err();
            assert!(
                err.to_string().contains("exists but is not a directory"),
                "unexpected error: {err}"
            );
        });
    }

    #[cfg_attr(miri, ignore = "count_entries reads real host directories")]
    #[test]
    fn count_entries_counts_files_and_dirs() {
        in_manifest_scope(|root| {
            let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();

            // The tempdir root carries its own control files; assert on deltas.
            let baseline = super::count_entries(root, proc_macro2::Span::call_site()).unwrap();
            resolver.write_file(&root.join("a").unwrap(), b"1").unwrap();
            resolver.create_dir_all(&root.join("b").unwrap()).unwrap();
            assert_eq!(
                super::count_entries(root, proc_macro2::Span::call_site()).unwrap(),
                baseline + 2
            );
        });
    }

    #[test]
    fn build_assets_reports_parse_errors_with_prefix() {
        let out_dir = GuardedPath::tempdir().unwrap();
        let err = in_manifest_scope(|_| {
            super::build_assets(
                "THIS IS NOT VALID DSL !!!",
                proc_macro2::Span::call_site(),
                out_dir.as_guarded_path(),
            )
            .expect_err("invalid DSL must fail")
        });
        assert!(
            err.to_string().contains("parse error"),
            "unexpected error: {err}"
        );
    }
}
