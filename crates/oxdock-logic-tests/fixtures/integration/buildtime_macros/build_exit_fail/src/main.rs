use oxdock_buildtime_macros::embed;

mod demo_assets {
    use super::*;

    embed!(DemoAssets);
}

use demo_assets::DemoAssets;

fn main() {
    let _ = DemoAssets::iter();
}
