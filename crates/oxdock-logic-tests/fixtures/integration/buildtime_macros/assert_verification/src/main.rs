use oxdock_buildtime_macros::embed;

// Mirrors the README quick-start script so the documented macro path is
// compiled and executed — assertions included — on every CI run.
embed! {
    name: VerifiedAssets,
    script: {
        ENV PROJECT=oxdock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}
        ECHO building-dist
        ASSERT_STDOUT building-dist
    },
    out_dir: "prebuilt",
}

fn main() {
    assert!(
        VerifiedAssets::get("dist/hello.txt").is_some(),
        "verified artifact must be embedded"
    );
}
