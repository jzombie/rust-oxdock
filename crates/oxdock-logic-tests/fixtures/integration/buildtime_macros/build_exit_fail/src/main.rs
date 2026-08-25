use oxdock_buildtime_macros::embed;

mod demo_assets {
    use super::*;

    embed! {
        name: DemoAssets,
        script: "EXIT 5",
        out_dir: "prebuilt",
    }
}

use demo_assets::DemoAssets;

fn main() {
    let _ = DemoAssets::iter();
}
