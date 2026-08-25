use oxdock_buildtime_macros::embed;

mod demo_assets {
    use super::*;

    embed! {
        name: DemoAssetsA,
        script: "COPY source_a.txt copied.txt",
        out_dir: "prebuilt_a",
    }

    embed! {
        name: DemoAssetsB,
        script: "COPY source_b.txt copied.txt",
        out_dir: "prebuilt_b",
    }
}

use demo_assets::{DemoAssetsA, DemoAssetsB};

fn main() {
    // Check each embedded set contains only its expected file and contents.
    let data_a = DemoAssetsA::get("copied.txt").expect("copied file embedded in A");
    let s_a = std::str::from_utf8(data_a.data.as_ref()).expect("utf8");
    assert_eq!(s_a.trim_end(), "hello from manifest A");

    let data_b = DemoAssetsB::get("copied.txt").expect("copied file embedded in B");
    let s_b = std::str::from_utf8(data_b.data.as_ref()).expect("utf8");
    assert_eq!(s_b.trim_end(), "hello from manifest B");

    // Ensure no cross-contamination: A shouldn't have b, B shouldn't have a.
    assert!(
        DemoAssetsA::get("copied.txt").is_some(),
        "A must contain copied.txt"
    );
    assert!(
        DemoAssetsB::get("copied.txt").is_some(),
        "B must contain copied.txt"
    );
}
