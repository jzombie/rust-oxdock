# oxdock-doc-tests

Executes README Rust examples as doctests.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Includes the root `README.md` and `oxdock/README.md` as rustdoc doctests so the
Rust code fences in the documentation are compiled and executed on every
`cargo test` run. This catches documentation drift without a separate test
harness.

## License

`oxdock-doc-tests` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
