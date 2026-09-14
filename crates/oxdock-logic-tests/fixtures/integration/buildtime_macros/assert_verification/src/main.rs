use oxdock_macros::oxdock_embed;

// Mirrors the README quick-start script so the documented macro path is
// compiled and executed — assertions included — on every CI run.
oxdock_embed! {
    name: VerifiedAssets,
    script: {
        ENV PROJECT=OxDock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        LET $body: STRING = READ dist/hello.txt
        ASSERT_EQ $body "Built with OxDock"
        ECHO building-dist
        ASSERT_CONTAINS stdout "building-dist"
    },
    out_dir: "prebuilt",
}

fn main() {
    // Same read-back shape documented in the README quick start: the
    // generated struct serves the asset straight from the binary.
    let file = VerifiedAssets::get("dist/hello.txt").expect("dist/hello.txt must be embedded");
    assert_eq!(file.data.as_ref(), b"Built with OxDock");
}
