fn main() {
    oxdock_buildtime_helpers::embed_assets(&oxdock_buildtime_helpers::EmbedSpec::new(
        "DemoAssets",
        oxdock_buildtime_helpers::DslSource::Inline("EXIT 5"),
    ))
    .expect("asset build failed");
}
