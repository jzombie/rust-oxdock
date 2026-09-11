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

#[test]
fn macro_nested_arithmetic_folds_and_runs() {
    // Folded-literal path through `emit_expr`: 2 * (2 * (2 + 3)) * 4 = 80.
    let steps = oxdock! {
        LET $x: INT = 2 * (2 * (2 + 3)) * 4
        WRITE nested.txt "{{ $x }}"
    };

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    assert_eq!(read_file(&root, "nested.txt"), "80");
}

#[test]
fn macro_dynamic_rpn_matches_runtime() {
    // RPN path through `emit_math_op`: vars, call, ordering, Inspect op.
    let steps = oxdock! {
        LET $size_str: STRING = ECHO 14
        LET $total: INT = INT($size_str) + 1
        LET $ratio: FLOAT = 1 + ($total * (2.5 - (4 / 2)))
        IF ($total * 2) > ($total + 10) {
            WRITE big.txt yes
        }
        IF $total == 15 {
            WRITE total.txt "{{ $total }}|{{ $ratio }}"
        }
    };

    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_steps(&root, &steps);
    // 14 + 1 = 15; 1 + (15 * (2.5 - 2)) = 8.5; 30 > 25 so big.txt exists.
    assert_eq!(read_file(&root, "total.txt"), "15|8.5");
    assert_eq!(read_file(&root, "big.txt"), "yes");
}

#[test]
fn macro_emitted_steps_equal_parsed_steps() {
    // Emit fidelity: macro expansion must equal runtime parsing, including
    // CompiledMath, Arithmetic fallback, calls, and Inspect ops.
    let via_macro = oxdock! {
        LET $t: INT = $total + INT($size_str)
        LET $ok: BOOL = INSPECT($p) == INSPECT($p)
        LET $n: INT = -$v * 2
        IF $t >= 10 {
            ECHO big
        }
    };
    let via_parse = oxdock_core::parse_script(
        "LET $t: INT = $total + INT($size_str)\nLET $ok: BOOL = INSPECT($p) == INSPECT($p)\nLET $n: INT = -$v * 2\nIF $t >= 10 {\nECHO big\n}\n",
    )
    .unwrap();
    assert_eq!(via_macro, via_parse);
}

#[test]
fn macro_and_parse_execute_identically() {
    // Dual-path runtime parity: the same script through `oxdock!`
    // expansion and through `parse_script` must produce identical files.
    let via_macro = oxdock! {
        LET $a: INT = 6
        LET $b: INT = 7
        LET $total: INT = $a * ($b + 1) - 10 / 2
        LET $ratio: FLOAT = $total + 0.5
        WRITE out.txt "{{ $total }}|{{ $ratio }}"
    };
    let via_parse = oxdock_core::parse_script(
        "LET $a: INT = 6\nLET $b: INT = 7\nLET $total: INT = $a * ($b + 1) - 10 / 2\nLET $ratio: FLOAT = $total + 0.5\nWRITE out.txt \"{{ $total }}|{{ $ratio }}\"\n",
    )
    .unwrap();

    let temp_macro = GuardedPath::tempdir().unwrap();
    let root_macro = temp_macro.as_guarded_path().clone();
    run_steps(&root_macro, &via_macro);

    let temp_parse = GuardedPath::tempdir().unwrap();
    let root_parse = temp_parse.as_guarded_path().clone();
    run_steps(&root_parse, &via_parse);

    // 6 * 8 - 5 = 43; 43 + 0.5 = 43.5.
    assert_eq!(read_file(&root_macro, "out.txt"), "43|43.5");
    assert_eq!(
        read_file(&root_macro, "out.txt"),
        read_file(&root_parse, "out.txt")
    );
}
