use oxdock_buildtime_macros::embed;

mod demo_assets {
    use super::*;

    embed! {
        name: DemoAssets,
        script: "WITH_IO [stdout=pipe:cap_env_txt] ECHO {{ env:CARGO_FEATURE_OXDOCK_TEST }}:{{ env:CARGO_CFG_TARGET_OS }}; WITH_IO [stdin=pipe:cap_env_txt] WRITE env.txt",
        out_dir: "prebuilt",
    }
}

use demo_assets::DemoAssets;

fn main() {
    let data = DemoAssets::get("env.txt").expect("env.txt should be embedded");
    let contents = std::str::from_utf8(data.data.as_ref()).expect("env.txt should be utf8");
    let mut parts = contents.trim().split(':');
    let feature = parts.next().unwrap_or_default();
    let target_os = parts.next().unwrap_or_default();
    assert_eq!(feature, "1");
    assert!(!target_os.is_empty(), "target os should be present");
}
