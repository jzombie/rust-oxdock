fn main() {
    let script = r#"
MKDIR data/inner
WRITE data/inner/a.txt alpha
WRITE data/b.txt beta
WITH_IO [stdout=pipe:cap_dir_hash] HASH_SHA256 data
WITH_IO [stdin=pipe:cap_dir_hash] WRITE dir_hash.txt
WITH_IO [stdout=pipe:cap_file_hash] HASH_SHA256 data/inner/a.txt
WITH_IO [stdin=pipe:cap_file_hash] WRITE file_hash.txt
"#;
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "SnapshotAssets",
            oxdock_buildtime_helpers::DslSource::Inline(script),
        )
        .subdir("prebuilt"),
    )
    .expect("embedded asset build failed");

    // Prepare-only twin: materializes into $OUT_DIR without a runtime module.
    oxdock_buildtime_helpers::prepare_assets(&oxdock_buildtime_helpers::PrepareSpec::new(oxdock_buildtime_helpers::DslSource::Inline(
        script,
    )))
    .expect("prepared asset build failed");
}
