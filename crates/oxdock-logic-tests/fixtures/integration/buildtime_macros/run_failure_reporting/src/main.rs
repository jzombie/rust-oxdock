use oxdock_macros::oxdock_embed;

oxdock_embed! {
    name: DemoAssets,
    script: {
        // Closed child env: the failing command must resolve through the
        // listed PATH so the failure is a status failure (the reporting
        // shape under test), never a resolution failure. Prelude position
        // is mandatory: INHERIT_ENV leads, WORKDIR follows.
        INHERIT_ENV [PATH]
        WORKDIR /
        RUN ["cargo", "--invalid-oxdock-flag-xyz"]
    },
    out_dir: "prebuilt",
}

fn main() {
    let _ = DemoAssets::iter();
}
