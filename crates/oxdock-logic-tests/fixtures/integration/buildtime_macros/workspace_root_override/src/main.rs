use oxdock_buildtime_macros::embed;

embed! {
    name: WorkspaceAssets,
    script: {
        WORKDIR /
        COPY client client_copy
        WORKDIR client_copy/dist
    },
    out_dir: "prebuilt",
}

fn main() {
    let file = WorkspaceAssets::get("version.txt").expect("workspace asset");
    let data = core::str::from_utf8(&file.data).expect("utf8 contents");
    assert_eq!(data.trim(), "workspace-root");
}
