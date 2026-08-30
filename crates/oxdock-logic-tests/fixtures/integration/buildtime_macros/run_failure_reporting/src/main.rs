use oxdock_macros::oxdock_embed;

oxdock_embed! {
    name: DemoAssets,
    script: {
        WORKDIR /
        RUN __oxdock_missing_command__
    },
    out_dir: "prebuilt",
}

fn main() {
    let _ = DemoAssets::iter();
}
