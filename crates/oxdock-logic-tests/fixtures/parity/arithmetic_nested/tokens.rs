# Parity fixture: this file is NOT compiled as Rust. It is lexed
# into Rust tokens, rendered back to DSL text, and its AST must match
# the twin dsl.txt exactly. Keep every line lexable as Rust tokens:
# no backticks, balanced quotes and brackets.
LET $x: INT = 2 * (2 * (2 + 3)) * 4
LET $t: INT = $total + INT($size_str)
LET $f: FLOAT = FLOAT("2.5") + 1
LET $ok: BOOL = $a < $b && $b < $c || $d == 1.0
LET $same: BOOL = INSPECT($p) == INSPECT($p)
LET $n: INT = -$v * 2
$total = $t * 2 - 1
IF $t >= 10 && $ok || $same {
    ECHO big
}
