use oxdock_buildtime_macros::embed;

// Mirrors the README quick-start script so the documented macro path is
// compiled and executed — assertions included — on every CI run.
embed! {
    name: VerifiedAssets,
    script: {
        ENV PROJECT=OxDock
        MKDIR dist
        WRITE dist/hello.txt Built with {{ env:PROJECT }}
        ASSERT_FILE dist/hello.txt Built with {{ env:PROJECT }}
        ECHO building-dist
        ASSERT_STDOUT building-dist
    },
    out_dir: "prebuilt",
}

fn main() {
    // Same read-back shape documented in the README quick start: the
    // generated struct serves the asset straight from the binary.
    let file = VerifiedAssets::get("dist/hello.txt").expect("dist/hello.txt must be embedded");
    assert_eq!(file.data.as_ref(), b"Built with OxDock");
}
