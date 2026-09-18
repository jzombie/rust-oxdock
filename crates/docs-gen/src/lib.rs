pub mod host;
pub mod io;
pub mod oxdock;
pub mod rust;

use anyhow::{Context, Result};
use oxdock_core::{Engine, ExecIo};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use oxdock_macros::oxdock;
use oxdock_process::default_process_manager;
#[allow(clippy::disallowed_types)]
use std::path::Path;

/// Execute the document pipeline through one DSL script: inherit the
/// host version, refresh generated inputs, sync per-member values,
/// assemble one `$files` manifest per target, then render every master
/// template through native OxDock commands. Order lives in the master
/// templates as `{{ $files.group.stem }}` placeholders; the value
/// returning host functions in [`host`] supply the pieces the DSL has
/// no primitives for (registry rendering, TOML metadata, strict JSON
/// encoding, placeholder-safe stems) while the script owns all
/// sequencing and all writes. Each stage below is a named `FUNC` so
/// the top level reads as the stage list.
#[allow(clippy::disallowed_types)]
pub fn run(repo_root: &Path) -> Result<()> {
    let root = GuardedPath::new_root(repo_root)?;

    let steps: Vec<oxdock_parser::Step> = oxdock! {
        modules: [DOCS],
        INHERIT_ENV [CRATE_VERSION]
        IMPORT [STD, DOCS]

        // Live-tree invariant: every WRITE below must land in the real
        // repo, so pin resolution to the build context up front. Without
        // this, a snapshot-backed resolver would silently materialize a
        // tempdir and the render would succeed while updating nothing.
        WORKSPACE LOCAL

        // Version for every expansion scope below.
        LET $version: STRING = WORKSPACE_VERSION()
        ENV CRATE_VERSION=$version

        // Workspace config: scopes, shared values, generated destinations.
        LET $cfg: MAP = LOAD_JSON("docs-gen.json")
        LET $gen: MAP = $cfg.generated
        LET $docs_global: MAP = LOAD_JSON($cfg.global_values)

        // Registry-derived inputs declared by the config.
        FUNC REFRESH_GENERATED($gen: MAP) {
            LET $index: STRING = DOCS::COMMAND_INDEX()
            WRITE $gen.command_index $index

            LET $body: STRING = DOCS::COMMAND_BODY()
            WRITE $gen.command_body $body

            LET $funcref: STRING = DOCS::FUNCTION_REFERENCE()
            WRITE $gen.function_reference $funcref
        }
        REFRESH_GENERATED($gen)

        // One member's values file from its manifest.
        FUNC SYNC_MEMBER($member: STRING) {
            ECHO "syncing values for {{ $member }}"

            LET $pkg: MAP = DOCS::CARGO_PACKAGE($member)
            IF $pkg.name != "" {
                LET $values_path: STRING = "{{ $member }}/.oxdock/template/values.json"
                LET $pt: STRING = PATH_TYPE($values_path)
                IF $pt == "file" {
                    LET $existing: MAP = LOAD_JSON($values_path)
                    LET $json: STRING = DOCS::PACKAGE_VALUES_JSON($pkg, $existing)
                    WRITE $values_path $json
                } ELSE {
                    LET $empty: MAP = {}
                    LET $fresh: STRING = DOCS::PACKAGE_VALUES_JSON($pkg, $empty)
                    WRITE $values_path $fresh
                }
            }
        }
        FOR $member: STRING IN DOCS::WORKSPACE_MEMBERS() {
            SYNC_MEMBER($member)
        }

        // Track one target name, failing on duplicates.
        FUNC NOTE_TARGET($seen: MAP, $name: STRING, $tj: STRING) {
            IF DOCS::HAS_KEY($seen, $name) {
                ECHO "duplicate target name '{{ $name }}' (in {{ $tj }})"
                EXIT 1
            }
            RETURN DOCS::MAP_SET($seen, $name, $tj)
        }

        // One target manifest as JSON: expand every fragment once.
        FUNC BUILD_MANIFEST($t: MAP, $docs_global: MAP, $docs_ctx: MAP, $version: STRING) {
            LET $manifest: MAP = {}
            FOR $group: STRING, $patterns: LIST IN $t.fragments {
                LET $group_map: MAP = {}
                FOR $pattern: STRING IN $patterns {
                    FOR $file_rel: STRING IN GLOB($pattern) {
                        LET $stem: STRING = DOCS::FILE_STEM($file_rel)
                        IF HAS_KEY($group_map, $stem) {
                            ECHO "target '{{ $t.name }}': '{{ $stem }}' matches more than one file; placeholders must resolve to exactly one"
                            EXIT 1
                        }
                        LET $raw: STRING = READ $file_rel
                        LET $expanded: STRING = EXPAND_FRAGMENT($raw, $docs_global, $docs_ctx, $version)
                        $group_map = DOCS::MAP_SET($group_map, $stem, $expanded)
                    }
                }
                $manifest = DOCS::MAP_SET($manifest, $group, $group_map)
            }
            RETURN TO_JSON($manifest)
        }
        LET $seen: MAP = {}
        FOR $scope: STRING IN $cfg.scopes {
            FOR $tj: STRING IN GLOB("{{ $scope }}/**/target.json") {
                LET $file: MAP = LOAD_JSON($tj)
                FOR $t: MAP IN $file.targets {
                    $seen = NOTE_TARGET($seen, $t.name, $tj)
                    IF DOCS::HAS_KEY($t, "globs") {
                        ECHO "target '{{ $t.name }}' still uses 'globs'; declare 'template' and grouped 'fragments' patterns instead"
                        EXIT 1
                    }
                    LET $docs_ctx: MAP = LOAD_JSON($t.values)
                    LET $json: STRING = BUILD_MANIFEST($t, $docs_global, $docs_ctx, $version)
                    WRITE "target/oxdock-docs/{{ $t.name }}.json" $json
                }
            }
        }

        // One master template expanded into its output.
        FUNC RENDER_TARGET($t: MAP, $docs_global: MAP) {
            ECHO "rendering {{ $t.name }} -> {{ $t.out }}"

            LET $docs_ctx: MAP = LOAD_JSON($t.values)
            LET $files: MAP = LOAD_JSON("target/oxdock-docs/{{ $t.name }}.json")

            WRITE $t.out ""
            WITH_IO [stdout=pipe:render] EXPAND $t.template
            WITH_IO [stdin=pipe:render] APPEND $t.out
        }
        FOR $rscope: STRING IN $cfg.scopes {
            FOR $rtj: STRING IN GLOB("{{ $rscope }}/**/target.json") {
                LET $rfile: MAP = LOAD_JSON($rtj)
                FOR $rt: MAP IN $rfile.targets {
                    RENDER_TARGET($rt, $docs_global)
                }
            }
        }
    };
    let mut fs_resolver = PathResolver::new_guarded(root.clone(), root.clone())?;
    fs_resolver.set_workspace_root(root.clone());
    let fs: Box<dyn WorkspaceFs> = Box::new(fs_resolver);
    let mut engine = Engine::new().with_io(ExecIo::new());
    engine.register_module(host::module());
    engine
        .run_steps_on(fs, &steps, default_process_manager())
        .context("render documents")?;
    eprintln!("docs rendered");
    Ok(())
}
