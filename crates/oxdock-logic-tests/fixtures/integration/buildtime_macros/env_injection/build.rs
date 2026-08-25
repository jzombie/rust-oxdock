fn main() {
    // Feature/cfg env vars are native to the build-script process; the DSL
    // reads them directly through BuiltinEnv.
    oxdock_buildtime_helpers::embed_assets(&oxdock_buildtime_helpers::EmbedSpec::new(
        "DemoAssets",
        oxdock_buildtime_helpers::DslSource::Inline(
            "WITH_IO [stdout=pipe:cap_env_txt] ECHO {{ env:CARGO_FEATURE_OXDOCK_TEST }}:{{ env:CARGO_CFG_TARGET_OS }}; WITH_IO [stdin=pipe:cap_env_txt] WRITE env.txt",
        ),
    ))
    .expect("asset build failed");
}
