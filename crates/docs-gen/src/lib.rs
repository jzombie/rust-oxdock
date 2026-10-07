pub mod docs_gen_engine;
pub mod plugins;

use anyhow::{Context, Result};
use oxdock_core::{Engine, ExecIo, HostModule};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_macros::oxdock;
use oxdock_process::default_process_manager;
#[allow(clippy::disallowed_types)]
use std::path::Path;

/// `run()`'s module set, written once.
///
/// Each entry is `(HeaderIdent, module_path)`. Every entry lands in
/// the `oxdock!` `modules:` list and `IMPORT` and is registered on the
/// `Engine`, so placeholder calls (`{{ MODULE::FUNC($var) }}` through
/// `EXPAND_TEMPLATE` snapshots) and script steps (bare or qualified
/// calls) resolve the same set. This list is this pipeline's
/// requirements, not engine defaults: the only module the engine
/// itself defaults to is `DOCS_GEN_ENGINE` (see [`pipeline_engine`]);
/// `OXDOCK`, `RUST`, and `MARKDOWN` are here because this pipeline
/// documents an OxDock Rust workspace, `DOCS` because deferred
/// placeholders (`DEFER`/`EXPAND_DEFERRED`) resolve in every template,
/// and a resume pipeline would list a different set.
///
/// External plugins never go here either: pass them to
/// [`run_with_plugins`], which registers them alongside this list.
/// Template and fragment placeholders resolve against the live
/// registered snapshot with no `IMPORT` scope check, so
/// `{{ CUSTOM::FUNC($var) }}` in any template works the moment the
/// module is registered. Script steps are different: the `oxdock!`
/// header governs them, so a custom *script* names its own modules
/// in its own header over [`pipeline_engine`].
///
/// A runtime `Vec` cannot be this list: `oxdock!` is a proc macro over
/// token streams at compile time and never sees runtime values. The
/// list therefore lives at token level, fanned out by
/// `pipeline_split!` into each use below. Paths thread through as
/// opaque token runs (never `:path`, which degrades across repeated
/// match cycles) and accumulate space separated, since a trailing
/// comma breaks the final match. Add or remove an entry here and the
/// header, the registrations, and the pinning test follow.
macro_rules! pipeline_modules {
    ($final:ident [$($args:tt)*]) => {
        pipeline_split! {
            $final ; [$($args)*] ;
            (DOCS_GEN_ENGINE, crate::docs_gen_engine),
            (DOCS, crate::plugins::docs),
            (OXDOCK, crate::plugins::oxdock),
            (RUST, crate::plugins::rust),
            (MARKDOWN, oxdock_markdown_plugin)
        }
    };
}

/// Fan one `pipeline_modules!` list out to a `$final!` callback as
/// `{ [args] ; [header idents] ; [(paths)] }`.
macro_rules! pipeline_split {
    ($final:ident ; [$($args:tt)*] ; $(($name:ident, $first:tt $(:: $rest:tt)*)),*) => {
        pipeline_split!(@acc $final ; [$($args)*] ; [] ; [] ; $(($name, $first $(:: $rest)*)),*)
    };
    (@acc $final:ident ; [$($args:tt)*] ; [$($hn:ident),*] ; [$($abag:tt)*]) => {
        $final! { [$($args)*] ; [$($hn),*] ; [$($abag)*] }
    };
    (@acc $final:ident ; [$($args:tt)*] ; [$($hn:ident),*] ; [$($abag:tt)*] ; ($name:ident, $first:tt $(:: $rest:tt)*) $(, $($tail:tt)*)?) => {
        pipeline_split!(@acc $final ; [$($args)*] ; [$($hn,)* $name] ; [$($abag)* ($first $(:: $rest)*)] $(; $($tail)*)?)
    };
}

/// Emit the `oxdock!` steps with the generated header plus `$body`.
macro_rules! pipeline_emit_steps {
    ([$($body:tt)*] ; [$($hn:ident),*] ; [$($abag:tt)*]) => {
        oxdock! {
            modules: [$($hn),*],
            INHERIT_ENV [CRATE_VERSION]
            IMPORT [STD, $($hn),*]
            $($body)*
        }
    };
}

/// Emit one `register_module` per pipeline module path.
macro_rules! pipeline_emit_register {
    ([$engine:expr] ; [$($hn:ident),*] ; [$(( $first:tt $(:: $rest:tt)* ))*]) => {{
        $($engine.register_module($first $(:: $rest)*::module());)*
    }};
}

/// Emit the header idents as strings, so the pinning test can compare
/// the DSL visible set against the registered set.
#[cfg(test)]
macro_rules! pipeline_emit_header_names {
    ([] ; [$($hn:ident),*] ; [$($abag:tt)*]) => {
        [$(stringify!($hn)),*]
    };
}

/// Execute the document pipeline through one DSL script: inherit the
/// host version, refresh generated inputs, sync per-member values,
/// assemble one `$files` manifest per target, then render every master
/// template through native OxDock commands. Order lives in the master
/// templates as `{{ $files.group.stem }}` placeholders; the value
/// returning engine builtins in [`docs_gen_engine`] supply the pieces the DSL
/// has no primitives for (registry rendering, TOML metadata, strict JSON
/// encoding, placeholder-safe stems) while the script owns all
/// sequencing and all writes. Each stage below is a named `FUNC` so
/// the top level reads as the stage list.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    run_with_plugins(repo_root, vec![])
}
/// Execute the document pipeline with extra modules registered
/// alongside this pipeline's set.
///
/// External plugins arrive here, never in `pipeline_modules!`: any
/// `{{ CUSTOM::FUNC($var) }}` placeholder in a template or fragment
/// resolves through the live registered snapshot, so no header or
/// script change is needed. Custom *scripts* are a separate path:
/// build them over [`pipeline_engine`] with their own `oxdock!`
/// header naming their own modules.
#[allow(clippy::disallowed_types)]
pub fn run_with_plugins(
    repo_root: &Path,
    extra_modules: Vec<HostModule<oxdock_process::DefaultProcessManager>>,
) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;

    let steps: Vec<oxdock_parser::Step> = pipeline_modules!(pipeline_emit_steps [
        // Live-tree invariant: every WRITE below must land in the real
        // repo, so pin resolution to the build context up front. Without
        // this, a snapshot-backed resolver would silently materialize a
        // tempdir and the render would succeed while updating nothing.
        WORKSPACE LOCAL

        // Version for every expansion scope below.
        LET $version: STRING = RUST::WORKSPACE_VERSION()
        ENV CRATE_VERSION=$version

        // Citation metadata for expansion scopes. Workspace author,
        // license, and repository come from the root manifest here, so
        // citation templates never copy them into checked-in files.
        LET $workspace_pkg: MAP<ANY> = RUST::WORKSPACE_PACKAGE()

        // Workspace config: scopes, shared values, generated destinations.
        LET $cfg: MAP<ANY> = LOAD_JSON("docs-gen.json")
        LET $gen: LIST<ANY> = $cfg.generated

        // Provider conventions: every key below is optional. Absent
        // keys keep the OxDock project defaults, so the current
        // workspace config behaves exactly as before.
        LET $policy: STRING = "fail_on_duplicate"
        IF HAS_KEY($cfg, "merge_policy") {
            $policy = $cfg.merge_policy
        }
        LET $staging: STRING = "target/oxdock-docs"
        IF HAS_KEY($cfg, "staging_dir") {
            $staging = $cfg.staging_dir
        }
        LET $vkey: STRING = "CRATE_VERSION"
        IF HAS_KEY($cfg, "version_key") {
            $vkey = $cfg.version_key
        }
        LET $vpkg: STRING = "workspace_pkg"
        IF HAS_KEY($cfg, "vars") {
            LET $vars: MAP<ANY> = $cfg.vars
            IF HAS_KEY($vars, "workspace_pkg") {
                $vpkg = $vars.workspace_pkg
            }
        }
        LET $vglobal: STRING = "docs_global"
        LET $vctx: STRING = "docs_ctx"
        IF HAS_KEY($cfg, "vars") {
            LET $vars: MAP<ANY> = $cfg.vars
            IF HAS_KEY($vars, "global") {
                $vglobal = $vars.global
            }
            IF HAS_KEY($vars, "ctx") {
                $vctx = $vars.ctx
            }
        }

        // Shared values: one path or a path list, merged in order.
        // STD::MERGE_MAPS validates the policy and names duplicates.
        LET $vpaths: LIST<ANY> = []
        IF TYPE_OF($cfg.global_values) == "LIST" {
            $vpaths = $cfg.global_values
        } ELSE {
            LIST_APPEND $vpaths $cfg.global_values
        }
        LET $vmaps: LIST<ANY> = []
        FOR $vp: STRING IN $vpaths {
            LET $one: MAP<ANY> = LOAD_JSON($vp)
            LIST_APPEND $vmaps $one
        }
        LET $docs_global: MAP<ANY> = STD::MERGE_MAPS($vmaps, $policy)

        // Registry-derived inputs declared by the config: one entry
        // per generated artifact, dispatched by key. An unknown key
        // fails listing the known ones instead of rendering empty.
        FUNC REFRESH_GENERATED($gen: LIST<ANY>) {
            FOR $entry: MAP<ANY> IN $gen {
                LET $key: STRING = $entry.key
                LET $text: STRING = OXDOCK::GENERATED($key)
                WRITE $entry.out $text
            }
        }
        REFRESH_GENERATED($gen)

        // One member's values file from its manifest.
        FUNC SYNC_MEMBER($member: STRING) {
            ECHO "syncing values for {{ $member }}"

            LET $pkg: MAP<ANY> = RUST::CARGO_PACKAGE($member)
            IF $pkg.name != "" {
                LET $values_path: STRING = "{{ $member }}/.oxdock/template/values.json"
                LET $pt: STRING = PATH_TYPE($values_path)
                IF $pt == "file" {
                    LET $existing: MAP<ANY> = LOAD_JSON($values_path)
                    LET $json: STRING = RUST::PACKAGE_VALUES_JSON($pkg, $existing)
                    WRITE $values_path $json
                } ELSE {
                    LET $empty: MAP<ANY> = {}
                    LET $fresh: STRING = RUST::PACKAGE_VALUES_JSON($pkg, $empty)
                    WRITE $values_path $fresh
                }
            }
        }
        FOR $member: STRING IN RUST::WORKSPACE_MEMBERS() {
            SYNC_MEMBER($member)
        }

        // Track one target name, failing on duplicates.
        FUNC NOTE_TARGET($seen: MAP<ANY>, $name: STRING, $tj: STRING) {
            IF HAS_KEY($seen, $name) {
                ECHO "duplicate target name '{{ $name }}' (in {{ $tj }})"
                EXIT 1
            }
            RETURN MAP_SET($seen, $name, $tj)
        }

        // One target manifest as JSON: expand every fragment once.
        FUNC BUILD_MANIFEST($t: MAP<ANY>, $docs_global: MAP<ANY>, $docs_ctx: MAP<ANY>, $version: STRING, $workspace_pkg: MAP<ANY>, $vglobal: STRING, $vctx: STRING, $vkey: STRING, $vpkg: STRING) {
            LET $manifest: MAP<ANY> = {}
            FOR $group: STRING, $patterns: LIST<ANY> IN $t.fragments {
                LET $group_map: MAP<ANY> = {}
                FOR $pattern: STRING IN $patterns {
                    FOR $file_rel: STRING IN GLOB($pattern) {
                        LET $stem: STRING = DOCS_GEN_ENGINE::FILE_STEM($file_rel)
                        IF HAS_KEY($group_map, $stem) {
                            ECHO "target '{{ $t.name }}': '{{ $stem }}' matches more than one file; placeholders must resolve to exactly one"
                            EXIT 1
                        }
                        LET $raw: STRING = READ $file_rel
                        // Scope assembly lives in the script, not the
                        // engine: each pipeline maps its own variable
                        // and env conventions here.
                        LET $scope: MAP<ANY> = {}
                        $scope = MAP_SET($scope, $vglobal, $docs_global)
                        $scope = MAP_SET($scope, $vctx, $docs_ctx)
                        $scope = MAP_SET($scope, $vpkg, $workspace_pkg)
                        LET $envmap: MAP<ANY> = {}
                        $envmap = MAP_SET($envmap, $vkey, $version)
                        LET $expanded: STRING = DOCS_GEN_ENGINE::EXPAND_TEMPLATE($raw, $scope, $envmap)
                        $group_map = MAP_SET($group_map, $stem, $expanded)
                    }
                }
                $manifest = MAP_SET($manifest, $group, $group_map)
            }
            RETURN TO_JSON($manifest)
        }
        LET $seen: MAP<ANY> = {}
        FOR $scope: STRING IN $cfg.scopes {
            FOR $tj: STRING IN GLOB("{{ $scope }}/**/target.json") {
                LET $file: MAP<ANY> = LOAD_JSON($tj)
                FOR $t: MAP<ANY> IN $file.targets {
                    LET $tname: STRING = $t.name
                    $seen = NOTE_TARGET($seen, $tname, $tj)
                    IF HAS_KEY($t, "globs") {
                        ECHO "target '{{ $t.name }}' still uses 'globs'; declare 'template' and grouped 'fragments' patterns instead"
                        EXIT 1
                    }
                    LET $values_path: STRING = $t.values
                    LET $docs_ctx: MAP<ANY> = LOAD_JSON($values_path)
                    LET $json: STRING = BUILD_MANIFEST($t, $docs_global, $docs_ctx, $version, $workspace_pkg, $vglobal, $vctx, $vkey, $vpkg)
                    WRITE "{{ $staging }}/{{ $t.name }}.json" $json
                }
            }
        }

        // One master template expanded into its output.
        FUNC READ_TEXT($path: STRING) {
            LET $text: STRING = READ $path
            RETURN $text
        }
        FUNC RENDER_TARGET($t: MAP<ANY>, $docs_global: MAP<ANY>, $staging: STRING) {
            ECHO "rendering {{ $t.name }} -> {{ $t.out }}"

            LET $values_path: STRING = $t.values
            LET $docs_ctx: MAP<ANY> = LOAD_JSON($values_path)
            LET $files: MAP<ANY> = LOAD_JSON("{{ $staging }}/{{ $t.name }}.json")

            WRITE $t.out ""
            LET $render: PIPE
            WITH_IO [stdout=$render] EXPAND $t.template
            WITH_IO [stdin=$render] APPEND $t.out
        }
        // Pass 2 lives in its own FUNC because declarations and call
        // expressions after WITH_IO steps do not bind in the same
        // scope: pass 1 stays byte-identical above, then this reads the
        // rendered output back, dispatches deferred sentinels against
        // the full document, and rewrites it. Outputs without sentinels
        // round-trip unchanged.
        FUNC POST_TARGET($t: MAP<ANY>) {
            LET $out_path: STRING = $t.out
            LET $full_text: STRING = READ_TEXT($out_path)
            LET $toc_final: STRING = DOCS::EXPAND_DEFERRED($full_text)
            WRITE $t.out $toc_final
        }
        FOR $rscope: STRING IN $cfg.scopes {
            FOR $rtj: STRING IN GLOB("{{ $rscope }}/**/target.json") {
                LET $rfile: MAP<ANY> = LOAD_JSON($rtj)
                FOR $rt: MAP<ANY> IN $rfile.targets {
                    RENDER_TARGET($rt, $docs_global, $staging)
                    POST_TARGET($rt)
                }
            }
        }
    ]);
    let mut fs_resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
    fs_resolver.set_workspace_root(root.clone());
    let fs: Box<dyn WorkspaceFs> = Box::new(fs_resolver);
    let mut engine = Engine::new().with_io(ExecIo::new());
    pipeline_modules!(pipeline_emit_register[engine]);
    for module in extra_modules {
        engine.register_module(module);
    }
    engine
        .run_steps_on(fs, &steps, default_process_manager())
        .context("render documents")?;
    eprintln!("docs rendered");
    Ok(())
}

/// Engine with the engine default registered, for downstream
/// pipelines running their own scripts.
///
/// The only default is `DOCS_GEN_ENGINE`: the file-set assembler's
/// own primitives, needed by every pipeline built on this engine.
/// Domain modules are per-pipeline choices, registered by the
/// pipeline itself: custom scripts declare their own `oxdock!`
/// header explicitly and register `OXDOCK`, `RUST`, `MARKDOWN`, or
/// their own modules on top of this engine. `run()` below is one
/// such pipeline, for OxDock Rust workspaces.
pub fn pipeline_engine() -> Engine<oxdock_process::DefaultProcessManager> {
    let mut engine = Engine::new().with_io(ExecIo::new());
    engine.register_module(docs_gen_engine::module());
    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_registrations_cover_header_modules() {
        // The header idents and run()'s registered set come from one
        // token list. Drift in either direction fails here: a header
        // ident with no registration, or a registration with no
        // header ident. STD is always present and never listed.
        // `pipeline_engine()` is deliberately not used: it carries
        // only the DOCS_GEN_ENGINE default, while this pins run()'s
        // full pipeline set.
        let header: Vec<&str> = pipeline_modules!(pipeline_emit_header_names []).to_vec();
        assert!(!header.is_empty(), "pipeline must declare modules");
        let mut engine = Engine::new().with_io(ExecIo::new());
        pipeline_modules!(pipeline_emit_register[engine]);
        let table = engine.module_table();
        for name in &header {
            assert!(
                table.modules.contains_key(*name),
                "header module '{name}' is not registered",
            );
        }
        let mut registered: Vec<&str> = table
            .modules
            .keys()
            .map(String::as_str)
            .filter(|name| *name != "STD")
            .collect();
        registered.sort_unstable();
        let mut expected = header.clone();
        expected.sort_unstable();
        assert_eq!(registered, expected, "registered set drifted from header");
    }
}
