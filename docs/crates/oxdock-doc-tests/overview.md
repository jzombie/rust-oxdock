Includes the root `README.md` as a rustdoc doctest so the Rust code fences in
the documentation are compiled and executed on every `cargo test` run. This
catches documentation drift without a separate test harness.
