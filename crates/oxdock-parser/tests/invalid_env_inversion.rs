mod common;
use common::mock_lower;

use oxdock_parser::parse_script;

#[test]
fn disallow_old_equality_syntax() {
    // The old env:KEY==value syntax is no longer supported.
    // Use eq(KEY, value) or neq(KEY, value) instead.
    let scripts = ["[env:A==1] RUN echo no", "[env:A!=1] RUN echo no"];
    for script in scripts {
        parse_script(script, mock_lower).expect_err("old env:KEY==value syntax should be rejected");
    }
}
