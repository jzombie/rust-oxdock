use oxdock_macros::oxdock_embed;

mod demo_assets {
    use super::*;

    oxdock_embed! {
        name: DemoAssets,
        script: "EXIT 5",
        out_dir: "prebuilt",
    }
}

use demo_assets::DemoAssets;

fn main() {
    let _ = DemoAssets::iter();
}
