//! Real-macro corpus: every case here invokes the actual `oxdock!`
//! proc macro (real rustc token streams, real spans), never a
//! hand-parsed `TokenStream`. Scripts self-assert inside the DSL where
//! possible, so a passing test means expansion, lowering, and runtime
//! all agreed; file-effect cases assert bytes on disk.

use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_macros::oxdock;

fn run_steps(root: &GuardedPath, steps: &[oxdock_parser::Step]) {
    run_steps_with_context_result_with_io(root, root, steps, ExecIo::new()).unwrap();
}

fn read_file(root: &GuardedPath, name: &str) -> String {
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&root.join(name).unwrap()).unwrap()
}

/// Macro-first guarantee: steps produced by the real `oxdock!` macro
/// must render to strings that reparse identically. The string form is
/// derived from the macro product here, never the primary input.
/// Comparison is content-exact: quoting flags are parse provenance
/// (Display defensively quotes values like `six.txt`, flipping the
/// flag on reparse), so they compare separately in
/// `macro_emitted_quoting_flags_survive` below, not here. The whole
/// script reparses at once so cross-step context (FUNC definitions for
/// later calls) survives the round trip.
fn assert_macro_string_roundtrip(steps: &[oxdock_parser::Step]) {
    let text = steps
        .iter()
        .map(|step| step.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    // SCRIPT-opaque table: the macro resolves bare calls to `SCRIPT::`
    // at expansion, so the reparse admits the qualifier the same way.
    let mut table = oxdock_core::std_module_table();
    table.modules.insert("SCRIPT".to_string(), None);
    let reparsed = oxdock_core::parse_script_with_modules(&text, table)
        .expect("macro output must reparse as string");
    assert_eq!(
        reparsed.len(),
        steps.len(),
        "macro script split on reparse: {text}"
    );
    for (index, (got, want)) in reparsed.iter().zip(steps.iter()).enumerate() {
        assert!(
            kind_covered(&got.kind),
            "reparsed kind must be classified: {text}"
        );
        assert!(
            step_content_eq(got, want),
            "step {index} differs after reparse: {text}"
        );
    }
}

#[test]
fn macro_emitted_quoting_flags_survive() {
    // Emitter fidelity, directly on the macro product: source quoting
    // must survive expansion (dropping it changes runtime typing: an
    // unquoted `200` in ASSERT position converts to INT).
    let steps = oxdock! {
        ASSERT_EQ $x "200"
        ECHO bare
    };
    match &steps[0].kind {
        oxdock_parser::StepKind::AssertEq {
            expected: Some(arg),
            ..
        } => {
            assert_eq!(arg, &oxdock_parser::Arg::String("200".to_string(), true));
        }
        other => panic!("expected AssertEq, got {:?}", other),
    }
    match &steps[1].kind {
        oxdock_parser::StepKind::Echo(arg) => {
            assert_eq!(arg, &oxdock_parser::Arg::String("bare".to_string(), false));
        }
        other => panic!("expected Echo, got {:?}", other),
    }
}

fn arg_content_eq(a: &oxdock_parser::Arg, b: &oxdock_parser::Arg) -> bool {
    use oxdock_parser::Arg;
    match (a, b) {
        (Arg::String(x, _), Arg::String(y, _)) => x == y,
        (Arg::Expr(x), Arg::Expr(y)) => x == y,
        (Arg::Parts(x), Arg::Parts(y)) => {
            x.len() == y.len()
                && x.iter().zip(y.iter()).all(|(p, q)| match (p, q) {
                    (oxdock_parser::ArgPart::Text(s, _), oxdock_parser::ArgPart::Text(t, _)) => {
                        s == t
                    }
                    (oxdock_parser::ArgPart::Expr(s), oxdock_parser::ArgPart::Expr(t)) => s == t,
                    _ => false,
                })
        }
        _ => false,
    }
}

fn opt_arg_eq(a: &Option<oxdock_parser::Arg>, b: &Option<oxdock_parser::Arg>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => arg_content_eq(x, y),
        _ => false,
    }
}

fn target_eq(a: &oxdock_parser::AssertTarget, b: &oxdock_parser::AssertTarget) -> bool {
    use oxdock_parser::AssertTarget;
    match (a, b) {
        (AssertTarget::Value(x), AssertTarget::Value(y)) => arg_content_eq(x, y),
        _ => a == b,
    }
}

fn steps_content_eq(a: &[oxdock_parser::Step], b: &[oxdock_parser::Step]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| step_content_eq(x, y))
}

#[test]
fn macro_block_dedent_self_asserts() {
    let steps = oxdock! {
        LET $prompt: STRING = """
            SELECT *
            FROM users
            """
        ASSERT_EQ $prompt "SELECT *\nFROM users"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_block_variants_self_assert() {
    let steps = oxdock! {
        LET $inline: STRING = """hello
            world
            """
        ASSERT_EQ $inline "hello\nworld"
        LET $empty: STRING = """"""
        ASSERT_EQ $empty ""
        LET $escaped: STRING = """
            code \"\"\" here
            """
        ASSERT_EQ $escaped "code \"\"\" here"
        LET $blank: STRING = """
            outer

            tail
            """
        ASSERT_EQ $blank "outer\n\ntail"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_block_interpolation_self_asserts() {
    let steps = oxdock! {
        LET $table: STRING = "users"
        LET $query: STRING = """
            SELECT * FROM {{ $table }}
            WHERE active
            """
        ASSERT_EQ $query "SELECT * FROM users\nWHERE active"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_let_scalars_and_collections_write_file() {
    let steps = oxdock! {
        LET $name: STRING = "ada"
        LET $n: INT = 42
        LET $pi: FLOAT = 3.5
        LET $flag: BOOL = true
        LET $items: LIST<STRING> = ["a", "b"]
        WRITE scalars.txt "{{ $name }}|{{ $n }}|{{ $pi }}|{{ $flag }}|{{ $items.0 }}{{ $items.1 }}"
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "scalars.txt"), "ada|42|3.5|true|ab");
}

#[test]
fn macro_if_else_for_control_flow() {
    let steps = oxdock! {
        LET $total: INT = 0
        FOR $i: INT IN [1, 2, 3] {
            $total = $total + $i
        }
        ASSERT_EQ $total 6
        IF $total == 6 {
            WRITE six.txt yes
        } ELSE {
            WRITE six.txt no
        }
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "six.txt"), "yes");
}

#[test]
fn macro_guarded_write_only_runs_when_set() {
    let steps = oxdock! {
        INHERIT_ENV [OXDOCK_CORPUS_FLAG]
        [env:OXDOCK_CORPUS_FLAG] WRITE corpus-gated.txt ran
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let mut io = ExecIo::new();
    io.insert_inherit_env("OXDOCK_CORPUS_FLAG", "1");
    run_steps_with_context_result_with_io(&root, &root, &steps, io).unwrap();
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "corpus-gated.txt"), "ran");
}

#[test]
fn macro_env_block_value_round_trips() {
    let steps = oxdock! {
        ENV GREETING = """
            hi there
            """
        WRITE env.txt "{{ env:GREETING }}"
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "env.txt"), "hi there");
}

#[test]
fn macro_assert_block_expected() {
    let steps = oxdock! {
        LET $got: STRING = "line one\nline two"
        ASSERT_EQ $got """
            line one
            line two
            """
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_while_break_continue() {
    let steps = oxdock! {
        LET $i: INT = 0
        WHILE $i < 10 {
            $i = $i + 1
            IF $i == 2 {
                CONTINUE
            }
            IF $i == 5 {
                BREAK
            }
        }
        ASSERT_EQ $i 5
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_func_call_return() {
    let steps = oxdock! {
        FUNC ADD($a: INT, $b: INT) {
            RETURN $a + $b
        }
        LET $s: INT = ADD(1, 2)
        ASSERT_EQ $s 3
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_map_and_keypath() {
    let steps = oxdock! {
        LET $m: MAP<name: STRING, city: STRING> = {name: "ada", city: "london"}
        ASSERT_EQ $m.name "ada"
        ASSERT_EQ $m.city "london"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_bool_logic_and_comparison() {
    let steps = oxdock! {
        LET $ok: BOOL = true && !false
        ASSERT_EQ $ok true
        IF 2 + 3 == 5 {
            WRITE five.txt yes
        }
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "five.txt"), "yes");
}

#[test]
fn macro_timeout_runs_fast_command() {
    let steps = oxdock! {
        TIMEOUT 30s ECHO quick
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_async_await_task() {
    let steps = oxdock! {
        LET $t: HANDLE = ASYNC ECHO background
        AWAIT $t
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_with_io_pipe_round_trip() {
    let steps = oxdock! {
        LET $p: PIPE
        WITH_IO [stdout=$p] ECHO piped
        WITH_IO [stdin=$p] READ_LINE $line
        ASSERT_EQ $line "piped"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_isolated_runs_quietly() {
    let steps = oxdock! {
        ISOLATED {
            WRITE iso.txt yes
        }
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_macro_string_roundtrip(&steps);
    assert_eq!(read_file(&root, "iso.txt"), "yes");
}

#[test]
fn macro_type_alias_and_list_append() {
    let steps = oxdock! {
        TYPE PERSON = MAP<name: STRING>
        LET $p: PERSON = {name: "ada"}
        ASSERT_EQ $p.name "ada"
        LET $l: LIST<STRING> = ["a"]
        LIST_APPEND $l "b"
        ASSERT_EQ $l.1 "b"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_prompt_binds_default_off_terminal() {
    let steps = oxdock! {
        PROMPT $city "City?" "london"
        ASSERT_EQ $city "london"
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_std_call_through_import() {
    let steps = oxdock! {
        IMPORT [STD]
        LET $n: INT = STD::TEXT_LENGTH("hello")
        ASSERT_EQ $n 5
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_inspect_and_fresh_pipe() {
    let steps = oxdock! {
        LET $x: STRING = "hi"
        LET $info: MAP<ANY> = INSPECT($x)
        ASSERT_EQ $info.type "STRING"
        LET $p: PIPE
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
}

#[test]
fn macro_sleep_and_exit_code() {
    let steps = oxdock! {
        SLEEP 1ms
    };
    let temp = GuardedPath::tempdir().unwrap();
    run_steps(temp.as_guarded_path(), &steps);
    assert_macro_string_roundtrip(&steps);
    let failing = oxdock! {
        EXIT 3
    };
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let io = ExecIo::new();
    assert!(
        run_steps_with_context_result_with_io(&root, &root, &failing, io).is_err(),
        "EXIT must fail the run"
    );
}

/// Coverage gate twin to the fuzzer inventory: every `StepKind`
/// variant appears here with no wildcard, so a new variant fails
/// compilation until classified. The comparison below stays total
/// over classified pairs; unclassified pairs compare false and fail
/// loudly at runtime.
#[allow(clippy::match_same_arms)]
fn kind_covered(kind: &oxdock_parser::StepKind) -> bool {
    use oxdock_parser::StepKind;
    match kind {
        StepKind::Workdir(..)
        | StepKind::Echo(..)
        | StepKind::Run(..)
        | StepKind::Mkdir(..)
        | StepKind::HashSha256 { .. }
        | StepKind::Exit(..)
        | StepKind::Env { .. }
        | StepKind::InheritEnv { .. }
        | StepKind::IsolatedInheritEnv { .. }
        | StepKind::Workspace(..)
        | StepKind::Copy { .. }
        | StepKind::CopyGit { .. }
        | StepKind::Symlink { .. }
        | StepKind::Install { .. }
        | StepKind::Archive { .. }
        | StepKind::Unarchive { .. }
        | StepKind::Ls(..)
        | StepKind::Cwd
        | StepKind::Read(..)
        | StepKind::ReadLine { .. }
        | StepKind::Prompt { .. }
        | StepKind::Write { .. }
        | StepKind::Append { .. }
        | StepKind::Remove { .. }
        | StepKind::Expand { .. }
        | StepKind::AssertEq { .. }
        | StepKind::AssertContains { .. }
        | StepKind::Sleep { .. }
        | StepKind::ListAppend { .. }
        | StepKind::Assign { .. }
        | StepKind::Set { .. }
        | StepKind::AssignCapture { .. }
        | StepKind::AwaitCapture { .. }
        | StepKind::AsyncBlock { .. }
        | StepKind::AssignAsync { .. }
        | StepKind::Await { .. }
        | StepKind::Cancel { .. }
        | StepKind::Timeout { .. }
        | StepKind::Isolated { .. }
        | StepKind::RemoteBlock { .. }
        | StepKind::RunExec { .. }
        | StepKind::FuncDef { .. }
        | StepKind::Call { .. }
        | StepKind::Return { .. }
        | StepKind::While { .. }
        | StepKind::Break
        | StepKind::Continue
        | StepKind::For { .. }
        | StepKind::If { .. }
        | StepKind::WithIo { .. }
        | StepKind::WithIoBlock { .. }
        | StepKind::TypeAlias { .. } => true,
    }
}

/// Content comparison over classified pairs. Mismatched kinds compare
/// false (loud runtime failure); unclassified kinds cannot reach here
/// unnoticed because `kind_covered` above refuses to compile them.
fn step_content_eq(a: &oxdock_parser::Step, b: &oxdock_parser::Step) -> bool {
    use oxdock_parser::StepKind;
    if a.guard != b.guard {
        return false;
    }
    match (&a.kind, &b.kind) {
        (StepKind::Workdir(x), StepKind::Workdir(y))
        | (StepKind::Echo(x), StepKind::Echo(y))
        | (StepKind::Run(x), StepKind::Run(y))
        | (StepKind::Mkdir(x), StepKind::Mkdir(y))
        | (StepKind::HashSha256 { path: x }, StepKind::HashSha256 { path: y })
        | (StepKind::Exit(x), StepKind::Exit(y)) => arg_content_eq(x, y),
        (StepKind::Env { key: kx, value: vx }, StepKind::Env { key: ky, value: vy }) => {
            kx == ky && arg_content_eq(vx, vy)
        }
        (StepKind::InheritEnv { keys: x }, StepKind::InheritEnv { keys: y }) => x == y,
        (StepKind::IsolatedInheritEnv { keys: x }, StepKind::IsolatedInheritEnv { keys: y }) => {
            x == y
        }
        (StepKind::Workspace(x), StepKind::Workspace(y)) => x == y,
        (
            StepKind::Copy {
                from: fx, to: tx, ..
            },
            StepKind::Copy {
                from: fy, to: ty, ..
            },
        ) => arg_content_eq(fx, fy) && arg_content_eq(tx, ty),
        (
            StepKind::CopyGit {
                rev: rx,
                from: fx,
                to: tx,
                ..
            },
            StepKind::CopyGit {
                rev: ry,
                from: fy,
                to: ty,
                ..
            },
        ) => arg_content_eq(rx, ry) && arg_content_eq(fx, fy) && arg_content_eq(tx, ty),
        (
            StepKind::Symlink {
                from: fx, to: tx, ..
            },
            StepKind::Symlink {
                from: fy, to: ty, ..
            },
        ) => arg_content_eq(fx, fy) && arg_content_eq(tx, ty),
        (StepKind::Install { from: fx, to: tx }, StepKind::Install { from: fy, to: ty }) => {
            arg_content_eq(fx, fy) && arg_content_eq(tx, ty)
        }
        (
            StepKind::Archive {
                src: sx, dst: dx, ..
            },
            StepKind::Archive {
                src: sy, dst: dy, ..
            },
        ) => arg_content_eq(sx, sy) && arg_content_eq(dx, dy),
        (
            StepKind::Unarchive {
                src: sx, dst: dx, ..
            },
            StepKind::Unarchive {
                src: sy, dst: dy, ..
            },
        ) => arg_content_eq(sx, sy) && arg_content_eq(dx, dy),
        (StepKind::Ls(x), StepKind::Ls(y)) => opt_arg_eq(x, y),
        (StepKind::Cwd, StepKind::Cwd) => true,
        (StepKind::Read(x), StepKind::Read(y)) => opt_arg_eq(x, y),
        (StepKind::ReadLine { var: x }, StepKind::ReadLine { var: y }) => x == y,
        (
            StepKind::Prompt {
                var: vx,
                message: mx,
                default: dx,
            },
            StepKind::Prompt {
                var: vy,
                message: my,
                default: dy,
            },
        ) => vx == vy && arg_content_eq(mx, my) && opt_arg_eq(dx, dy),
        (
            StepKind::Write {
                path: px,
                contents: cx,
            },
            StepKind::Write {
                path: py,
                contents: cy,
            },
        ) => arg_content_eq(px, py) && opt_arg_eq(cx, cy),
        (
            StepKind::Append {
                path: px,
                contents: cx,
            },
            StepKind::Append {
                path: py,
                contents: cy,
            },
        ) => arg_content_eq(px, py) && opt_arg_eq(cx, cy),
        (StepKind::Remove { path: x, .. }, StepKind::Remove { path: y, .. }) => {
            arg_content_eq(x, y)
        }
        (
            StepKind::Expand {
                path: px,
                overrides: ox,
            },
            StepKind::Expand {
                path: py,
                overrides: oy,
            },
        ) => {
            opt_arg_eq(px, py)
                && ox.len() == oy.len()
                && ox
                    .iter()
                    .zip(oy.iter())
                    .all(|((kx, vx), (ky, vy))| kx == ky && arg_content_eq(vx, vy))
        }
        (
            StepKind::AssertEq {
                hash: hx,
                actual: ax,
                expected: ex,
            },
            StepKind::AssertEq {
                hash: hy,
                actual: ay,
                expected: ey,
            },
        ) => hx == hy && target_eq(ax, ay) && opt_arg_eq(ex, ey),
        (
            StepKind::AssertContains {
                haystack: hx,
                needle: nx,
            },
            StepKind::AssertContains {
                haystack: hy,
                needle: ny,
            },
        ) => target_eq(hx, hy) && arg_content_eq(nx, ny),
        (StepKind::Sleep { duration: x }, StepKind::Sleep { duration: y }) => arg_content_eq(x, y),
        (
            StepKind::ListAppend { list: lx, item: ix },
            StepKind::ListAppend { list: ly, item: iy },
        ) => lx == ly && arg_content_eq(ix, iy),
        (
            StepKind::Assign {
                var: vx,
                decl_type: tx,
                expr: ex,
            },
            StepKind::Assign {
                var: vy,
                decl_type: ty,
                expr: ey,
            },
        ) => vx == vy && tx == ty && ex == ey,
        (StepKind::Set { var: vx, expr: ex }, StepKind::Set { var: vy, expr: ey }) => {
            vx == vy && ex == ey
        }
        (
            StepKind::AssignCapture {
                var: vx,
                decl_type: tx,
                cmd: cx,
            },
            StepKind::AssignCapture {
                var: vy,
                decl_type: ty,
                cmd: cy,
            },
        ) => vx == vy && tx == ty && step_content_eq(&step_shell(cx), &step_shell(cy)),
        (
            StepKind::AwaitCapture {
                out_var: ox,
                out_type: tx,
                task_var: vx,
            },
            StepKind::AwaitCapture {
                out_var: oy,
                out_type: ty,
                task_var: vy,
            },
        ) => ox == oy && tx == ty && vx == vy,
        (StepKind::AsyncBlock { body: x }, StepKind::AsyncBlock { body: y }) => {
            steps_content_eq(x, y)
        }
        (
            StepKind::AssignAsync {
                var: vx,
                decl_type: tx,
                body: bx,
            },
            StepKind::AssignAsync {
                var: vy,
                decl_type: ty,
                body: by,
            },
        ) => vx == vy && tx == ty && steps_content_eq(bx, by),
        (StepKind::Await { var: x }, StepKind::Await { var: y }) => x == y,
        (StepKind::Cancel { var: x }, StepKind::Cancel { var: y }) => x == y,
        (
            StepKind::Timeout {
                duration: dx,
                body: bx,
            },
            StepKind::Timeout {
                duration: dy,
                body: by,
            },
        ) => arg_content_eq(dx, dy) && steps_content_eq(bx, by),
        (StepKind::Isolated { body: x }, StepKind::Isolated { body: y }) => steps_content_eq(x, y),
        (
            StepKind::RemoteBlock {
                target: tx,
                vars: vx,
                env: ex,
                body: bx,
            },
            StepKind::RemoteBlock {
                target: ty,
                vars: vy,
                env: ey,
                body: by,
            },
        ) => tx == ty && vx == vy && ex == ey && steps_content_eq(bx, by),
        (StepKind::RunExec { argv: x }, StepKind::RunExec { argv: y }) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| arg_content_eq(a, b))
        }
        (
            StepKind::FuncDef {
                name: nx,
                params: px,
                body: bx,
            },
            StepKind::FuncDef {
                name: ny,
                params: py,
                body: by,
            },
        ) => nx == ny && px == py && steps_content_eq(bx, by),
        (StepKind::Call { name: nx, args: ax }, StepKind::Call { name: ny, args: ay }) => {
            nx == ny && ax == ay
        }
        (StepKind::Return { expr: x }, StepKind::Return { expr: y }) => x == y,
        (StepKind::While { cond: cx, body: bx }, StepKind::While { cond: cy, body: by }) => {
            cx == cy && steps_content_eq(bx, by)
        }
        (StepKind::Break, StepKind::Break) => true,
        (StepKind::Continue, StepKind::Continue) => true,
        (
            StepKind::For {
                key_var: kx,
                key_type: tx,
                var: vx,
                var_type: wx,
                in_expr: ix,
                body: bx,
            },
            StepKind::For {
                key_var: ky,
                key_type: ty,
                var: vy,
                var_type: wy,
                in_expr: iy,
                body: by,
            },
        ) => kx == ky && tx == ty && vx == vy && wx == wy && ix == iy && steps_content_eq(bx, by),
        (
            StepKind::If {
                cond: cx,
                then_body: tx,
                else_ifs: ex,
                else_body: bx,
            },
            StepKind::If {
                cond: cy,
                then_body: ty,
                else_ifs: ey,
                else_body: by,
            },
        ) => {
            cx == cy
                && steps_content_eq(tx, ty)
                && ex.len() == ey.len()
                && ex
                    .iter()
                    .zip(ey.iter())
                    .all(|((c1, b1), (c2, b2))| c1 == c2 && steps_content_eq(b1, b2))
                && match (bx, by) {
                    (None, None) => true,
                    (Some(x), Some(y)) => steps_content_eq(x, y),
                    _ => false,
                }
        }
        (
            StepKind::WithIo {
                bindings: bx,
                cmd: cx,
            },
            StepKind::WithIo {
                bindings: by,
                cmd: cy,
            },
        ) => bx == by && step_content_eq(&step_shell(cx), &step_shell(cy)),
        (StepKind::WithIoBlock { bindings: bx }, StepKind::WithIoBlock { bindings: by }) => {
            bx == by
        }
        (
            StepKind::TypeAlias {
                name: nx,
                target: tx,
            },
            StepKind::TypeAlias {
                name: ny,
                target: ty,
            },
        ) => nx == ny && tx == ty,
        _ => false,
    }
}

/// Wrap a boxed step kind in a shell step for content comparison.
fn step_shell(kind: &oxdock_parser::StepKind) -> oxdock_parser::Step {
    oxdock_parser::Step {
        guard: None,
        kind: kind.clone(),
        scope_enter: 0,
        scope_exit: 0,
    }
}
