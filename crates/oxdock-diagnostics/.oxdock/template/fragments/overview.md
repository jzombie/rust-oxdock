Generic keyed diagnostic transport for OxDock: stable dotted `key`
(for example `dsl.alias.duplicate_on_field` or `cli.feature_required`),
structured fields, optional span and hint, and an optional chained
source. No translation framework.

The crate owns only the transport, never domain templates. Each
template lives once with its owner:

- DSL spelling diagnostics live in `oxdock-parser/src/diagnostics.rs`
  (`dsl.spelling.*`, `dsl.alias.*` arity).
- DSL typing diagnostics live in
  `oxdock-core/src/exec/diagnostics.rs` (`dsl.map.*`,
  `dsl.alias.*` uniqueness).
- CLI diagnostics live in `oxdock-cli/src/errors.rs` (`cli.*`).

Rules:

- Call sites pass structured fields, never `format!` strings. The
  template lives once inside its owning constructor.
- Constructors take decoded fields, never raw mid-parse text. The
  transport sanitizes the internal `WS_SENTINEL` (`\u{1f}`) to a space,
  so untrusted input can never crash a release build. Tests assert the
  absence of the marker via `assert_no_internal_encodings`.
- The crate stays dependency-light (std only, never workspace crates),
  so parser, core, and CLI can all depend on it without a cycle.
  Conversion into `anyhow::Error` happens at call sites via
  `anyhow::Error::new`, never here. `Error::source` preserves chains
  across I/O, filesystem, process, and network boundaries.

See rustdoc for the item-level API.
