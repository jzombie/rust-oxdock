use oxdock_buildtime_macros::embed;

embed!(DemoAssets);

fn main() {
    let _ = DemoAssets::iter();
}
