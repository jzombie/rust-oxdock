fn main() {
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "DemoAssetsA",
            oxdock_buildtime_helpers::DslSource::Inline("COPY source_a.txt copied.txt"),
        )
        .subdir("prebuilt_a")
        .extra_input("source_a.txt"),
    )
    .expect("asset build failed for A");
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "DemoAssetsB",
            oxdock_buildtime_helpers::DslSource::Inline("COPY source_b.txt copied.txt"),
        )
        .subdir("prebuilt_b")
        .extra_input("source_b.txt"),
    )
    .expect("asset build failed for B");
}
