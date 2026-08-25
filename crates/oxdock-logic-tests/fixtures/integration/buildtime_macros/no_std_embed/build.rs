fn main() {
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "FirmwareAssets",
            oxdock_buildtime_helpers::DslSource::Inline("WORKDIR /\nMKDIR firmware\nWRITE firmware/version.txt 1.0.0-no-std"),
        )
        .subdir("prebuilt"),
    )
    .expect("firmware asset build failed");

    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "BrandingAssets",
            oxdock_buildtime_helpers::DslSource::Inline("WORKDIR /\nMKDIR branding\nWRITE branding/note.txt OxDock-no-std"),
        )
        .subdir("prebuilt_branding"),
    )
    .expect("branding asset build failed");
}
