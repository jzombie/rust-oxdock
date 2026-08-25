fn main() {
    oxdock_buildtime_helpers::embed_assets(&oxdock_buildtime_helpers::EmbedSpec::new(
        "DemoAssets",
        oxdock_buildtime_helpers::DslSource::Inline(
            "WORKDIR /\nRUN __oxdock_missing_command__",
        ),
    ))
    .expect("asset build failed");
}
