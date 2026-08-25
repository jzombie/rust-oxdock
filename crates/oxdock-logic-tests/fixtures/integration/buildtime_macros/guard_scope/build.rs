fn main() {
    let script = r#"
WORKDIR /

// The trybuild harness sets TEST_SCOPE=1 so this block executes during the test.
[env:TEST_SCOPE] {
    WORKDIR scoped
    WRITE inner.txt inside
    ENV SCOPE_FLAG=1

    [env:SCOPE_FLAG] {
        WORKDIR nested
        WRITE deep.txt nested
        ENV INNER_ONLY=1
    }

    WRITE after_nested.txt still-scoped

    [env:INNER_ONLY] WRITE leaked_inner.txt nope
}

WRITE outside.txt outside

[env:SCOPE_FLAG] WRITE leaked.txt nope
"#;
    oxdock_buildtime_helpers::embed_assets(
        &oxdock_buildtime_helpers::EmbedSpec::new(
            "GuardedAssets",
            oxdock_buildtime_helpers::DslSource::Inline(script),
        )
        .subdir("prebuilt"),
    )
    .expect("asset build failed");
}
