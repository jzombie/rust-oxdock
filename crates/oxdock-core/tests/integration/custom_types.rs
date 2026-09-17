//! End-to-end proof that host-defined types ride the same host export macros as
//! host-defined functions, registered through the `Engine` facade: a
//! `#[oxdock_type]` payload plus `#[oxdock_func]` markers flow through `LET`
//! declarations, host function arguments and returns, interpolation, and
//! `TYPES()`.

use indoc::indoc;
use oxdock_core::{Engine, EngineOutput, HostModule, OxDockFn, OxDockType, Value};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, WorkspaceFs};
use oxdock_func_macro::{oxdock_func, oxdock_type};
use oxdock_process::MockProcessManager;
use std::fmt;

/// Opaque label type.
///
/// The annotated struct IS the payload: the macro derives monomorphic heap
/// adapters over `Tag`, the same derivation the startup-registered types use.
#[oxdock_type(name = "TAG")]
#[derive(Debug, Clone, PartialEq)]
struct Tag(String);

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "tag:{}", self.0)
    }
}

/// Mint one opaque label.
#[oxdock_func(pure)]
fn make_tag() -> anyhow::Result<Value> {
    Ok(Value::mint_heap(Tag::descriptor(), Tag("demo".to_string())))
}

/// Read the payload back out through a descriptor-checked typed read.
#[oxdock_func(pure, returns = "STRING")]
fn read_tag(val: Value) -> anyhow::Result<Value> {
    let Some(tag) = val.read_heap::<Tag>(Tag::descriptor()) else {
        anyhow::bail!("READ_TAG() expects an opaque TAG value");
    };
    Ok(Value::string(tag.0.clone()))
}

fn run_with_tag_hosts(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let mut engine = Engine::new();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "TEST".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        types: vec![],
    });
    engine.run_script(root, script).map(|_| ())
}

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn read_trimmed(path: &GuardedPath) -> String {
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver
        .read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn custom_type_flows_through_declare_and_hosts() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, TEST]
        LET $t: TAG = MAKE_TAG()
        LET $s: STRING = READ_TAG($t)
        WRITE tag.txt "{{ $s }}::{{ $t }}"
        LET $ts: LIST = TYPES()
        WRITE types.txt "{{ $ts }}"
        LET $td: MAP = TYPE_DESCRIBE("TAG")
        WRITE tag-doc.txt "{{ $td.summary }}"
    "#};
    run_with_tag_hosts(&root, script).expect("custom type runs");
    assert_eq!(
        read_trimmed(&root.join("tag.txt").unwrap()),
        "demo::tag:demo"
    );
    assert!(
        read_trimmed(&root.join("types.txt").unwrap()).contains("TAG"),
        "TYPES() must list the registered custom type"
    );
    assert_eq!(
        read_trimmed(&root.join("tag-doc.txt").unwrap()),
        "Opaque label type."
    );
}

#[test]
fn unregistered_custom_type_fails_at_coercion() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_tag_hosts(&root, "IMPORT [STD, TEST]\nLET $x: NOPE = MAKE_TAG()\n")
        .expect_err("unknown custom type must fail");
    assert!(err.to_string().contains("unknown type `NOPE`"), "{err}");
}

#[test]
fn custom_value_rejected_by_builtin_declaration() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_tag_hosts(&root, "IMPORT [STD, TEST]\nLET $x: STRING = MAKE_TAG()\n")
        .expect_err("custom value must not coerce to STRING");
    assert!(err.to_string().contains("TypeMismatch"), "{err}");
}

/// A custom process manager flows through the same facade: the manager is
/// the engine's type parameter, so sandboxed harnesses stage host surface
/// exactly like default runs and read back the same run output.
#[test]
fn engine_runs_with_custom_process_manager() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
    let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
    let mut engine = Engine::<MockProcessManager>::new_custom();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "TEST".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        types: vec![],
    });
    let run = engine
        .run_script_on(
            fs,
            "IMPORT [STD, TEST]\nLET $t: TAG = MAKE_TAG()\n",
            MockProcessManager::default(),
        )
        .expect("custom manager runs");
    assert_eq!(run.bindings.len(), 1);
    assert_eq!(format!("{}", run.bindings["t"]), "tag:demo");
}

#[test]
fn queryable_container_reads_through_host_accessors() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, TEST]
        LET $m: MATRIX = MAKE_MATRIX()
        LET $cell: INT = MATRIX_GET($m, 0, 1)
        ASSERT_EQ $cell 2
        LET $rows: INT = MATRIX_ROWS($m)
        ASSERT_EQ $rows 2
    "#};
    let run = run_with_matrix_hosts(&root, script).expect("matrix reads run");
    assert_eq!(run.bindings.len(), 3);
    assert_eq!(format!("{}", run.bindings["m"]), "matrix[2x2]");
}

#[test]
fn container_accessors_reject_bad_reads() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_matrix_hosts(
        &root,
        "IMPORT [STD, TEST]\nLET $m: MATRIX = MAKE_MATRIX()\nLET $cell: INT = MATRIX_GET($m, 9, 9)\n",
    )
    .expect_err("out-of-bounds cell must fail");
    assert!(err.to_string().contains("out of bounds"), "{err}");
    let err = run_with_matrix_hosts(
        &root,
        "IMPORT [STD, TEST]\nLET $w: STRING = \"hi\"\nLET $cell: INT = MATRIX_GET($w, 0, 0)\n",
    )
    .expect_err("foreign value must fail");
    assert!(err.to_string().contains("expects a MATRIX value"), "{err}");
}

#[test]
fn custom_type_rejects_key_path_traversal() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_tag_hosts(
        &root,
        "IMPORT [STD, TEST]\nLET $t: TAG = MAKE_TAG()\nLET $x: STRING = $t.label\n",
    )
    .expect_err("key path into opaque type must fail");
    assert!(
        err.to_string().contains("Cannot traverse into scalar"),
        "{err}"
    );
}

#[test]
fn custom_type_rejects_for_iteration() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_tag_hosts(
        &root,
        "IMPORT [STD, TEST]\nLET $t: TAG = MAKE_TAG()\nFOR $x: TAG IN $t { ECHO hi }\n",
    )
    .expect_err("iteration over opaque type must fail");
    assert!(
        err.to_string()
            .contains("FOR loop requires a List or Map iterable"),
        "{err}"
    );
}

/// Queryable container with no literal syntax: a small integer grid.
/// Scripts read it only through host accessor functions; key paths and
/// `FOR` loops stay `LIST`/`MAP`-only (pinned below).
#[oxdock_type(name = "MATRIX")]
#[derive(Debug, Clone, PartialEq)]
struct Matrix(Vec<Vec<i64>>);

impl fmt::Display for Matrix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "matrix[{}x{}]",
            self.0.len(),
            self.0.first().map_or(0, Vec::len)
        )
    }
}

/// Mint a fixed 2x2 grid.
#[oxdock_func(pure)]
fn make_matrix() -> anyhow::Result<Value> {
    Ok(Value::mint_heap(
        Matrix::descriptor(),
        Matrix(vec![vec![1, 2], vec![3, 4]]),
    ))
}

/// Read one cell by row and column. Out-of-bounds indices are an error.
#[oxdock_func(pure, returns = "INT")]
fn matrix_get(board: Value, row: i64, col: i64) -> anyhow::Result<Value> {
    let Some(grid) = board.read_heap::<Matrix>(Matrix::descriptor()) else {
        anyhow::bail!("MATRIX_GET() expects a MATRIX value");
    };
    let cell = usize::try_from(row)
        .ok()
        .and_then(|r| grid.0.get(r))
        .and_then(|r| usize::try_from(col).ok().and_then(|c| r.get(c)))
        .copied()
        .ok_or_else(|| anyhow::anyhow!("MATRIX_GET() index [{row}, {col}] out of bounds"))?;
    Ok(Value::int(cell))
}

/// Count the rows.
#[oxdock_func(pure, returns = "INT")]
fn matrix_rows(board: Value) -> anyhow::Result<Value> {
    let Some(grid) = board.read_heap::<Matrix>(Matrix::descriptor()) else {
        anyhow::bail!("MATRIX_ROWS() expects a MATRIX value");
    };
    Ok(Value::int(grid.0.len() as i64))
}

fn run_with_matrix_hosts(root: &GuardedPath, script: &str) -> Result<EngineOutput, anyhow::Error> {
    let mut engine = Engine::new();
    engine.register_type::<Matrix>();
    engine.register_module(HostModule {
        name: "TEST".to_string(),
        funcs: vec![
            MakeMatrix::registration(),
            MatrixGet::registration(),
            MatrixRows::registration(),
        ],
        types: vec![],
    });
    engine.run_script(root, script)
}

/// Same descriptor name, different Rust payload: running with both must
/// fail loudly instead of aliasing two layouts.
#[oxdock_type(name = "TAG")]
#[derive(Debug, Clone, PartialEq)]
struct ImpostorTag(String);

impl std::fmt::Display for ImpostorTag {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "impostor:{}", self.0)
    }
}

#[test]
#[should_panic(expected = "duplicate function registration `TEST::MAKE_TAG`")]
fn duplicate_qualified_registration_panics() {
    let mut engine = Engine::new();
    let module = || HostModule {
        name: "TEST".to_string(),
        funcs: vec![MakeTag::registration()],
        types: vec![],
    };
    engine.register_module(module());
    engine.register_module(module());
}

#[test]
#[should_panic(expected = "duplicate function registration `STD::GLOB`")]
fn host_module_cannot_reclaim_std_name() {
    use oxdock_core::{FuncMeta, FuncParam, HostRegistration};
    use std::sync::Arc;
    let mut engine = Engine::new();
    engine.register_module(HostModule {
        name: "STD".to_string(),
        funcs: vec![HostRegistration::Pure {
            name: "GLOB".to_string(),
            meta: FuncMeta {
                name: "GLOB".to_string(),
                module: String::new(),
                kind: oxdock_core::FuncKind::HostPure,
                params: Some(vec![FuncParam {
                    name: "pattern".to_string(),
                    param_type: Some("STRING".to_string()),
                }]),
                returns: Some("LIST".to_string()),
                rpn: false,
                summary: "Shadow attempt.",
                docs: "Must never replace the builtin.",
            },
            func: Arc::new(|_| Ok(Value::string(String::new()))),
        }],
        types: vec![],
    });
}

#[test]
fn same_base_name_in_different_modules_coexists() {
    // `TEST::MAKE_TAG` and `OTHER::MAKE_TAG` are distinct entries: modules
    // isolate, so only the qualified name must be unique.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let mut engine = Engine::new();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "TEST".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        types: vec![],
    });
    engine.register_module(HostModule {
        name: "OTHER".to_string(),
        funcs: vec![MakeTag::registration()],
        types: vec![],
    });
    engine
        .run_script(
            &root,
            "IMPORT [TEST]\nLET $t: TAG = MAKE_TAG()\nLET $o: TAG = OTHER::MAKE_TAG()\nWRITE both.txt \"{{ $t }}::{{ $o }}\"\n",
        )
        .expect("distinct qualified names coexist");
    assert_eq!(
        read_trimmed(&root.join("both.txt").unwrap()),
        "tag:demo::tag:demo"
    );
}

#[test]
#[should_panic(expected = "already registered for a different descriptor")]
fn conflicting_payload_type_for_live_name_panics() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let mut engine = Engine::new();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "TEST".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        types: vec![],
    });
    engine.register_type::<ImpostorTag>();
    engine
        .run_script(&root, "IMPORT [STD, TEST]\nLET $t: TAG = MAKE_TAG()\n")
        .unwrap();
}
