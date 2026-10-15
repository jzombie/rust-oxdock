use oxdock_macros::oxdock_embed;

oxdock_embed! {
    name: DemoAssets,
    script: {
        // Closed child env: the seeded host PATH resolves the failing
        // command, so the failure is a status failure (the reporting
        // shape under test), never a resolution failure.
        WORKDIR /
        RUN ["cargo", "--invalid-oxdock-flag-xyz"]
    },
    out_dir: "prebuilt",
}

fn main() {
    let _ = DemoAssets::iter();
}
