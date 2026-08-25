use oxdock_buildtime_macros::embed;

embed! {
    name: GuardedAssets,
    script: {
        WORKDIR /

        // The trybuild harness sets TEST_SCOPE=1 so this block executes during the test.
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
    },
    out_dir: "prebuilt",
}

fn main() {
    assert!(
        GuardedAssets::get("scoped/inner.txt").is_some(),
        "scoped file must exist"
    );
    assert!(
        GuardedAssets::get("scoped/nested/deep.txt").is_some(),
        "nested scope file must exist"
    );
    assert!(
        GuardedAssets::get("scoped/after_nested.txt").is_some(),
        "workdir should revert to /scoped after nested block"
    );
    assert!(
        GuardedAssets::get("outside.txt").is_some(),
        "workdir should reset to root when block exits"
    );
    assert!(
        GuardedAssets::get("leaked.txt").is_none(),
        "env values set in scope must not leak outward"
    );
    assert!(
        GuardedAssets::get("scoped/leaked_inner.txt").is_none(),
        "nested env values must not leak to parent scope"
    );
}
