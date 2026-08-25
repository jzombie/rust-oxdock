fn main() {
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "WorkspaceAssets",
            oxdock_buildtime_helpers::DslSource::Inline(
                "WORKDIR /\nCOPY client client_copy\nWORKDIR client_copy/dist",
            ),
        )
        .subdir("prebuilt")
        .extra_input("client"),
    )
    .expect("asset build failed");
}
