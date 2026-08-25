use oxdock_buildtime_macros::embed;

embed!(GuardedAssets);

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
