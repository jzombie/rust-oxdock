# Examples

Runnable examples live with the package they demonstrate. Cargo only
builds `examples/` inside a package directory, so there are no targets
here, just pointers.

- `oxdock/examples/index.rs`: launcher that lists the runnable scripts
  beside it (`hangman.oxfile`, `guess.oxfile`) and runs the one you
  pick. Run with:
  `cargo run -p oxdock --example index`
- `crates/oxdock-cli/examples/tree_shell.rs`: builds a demo file tree
  with an embedded script, optionally dropping into a shell. Run with:
  `cargo run -p oxdock-cli --example tree_shell`
  (`-- --shell` to drop into the shell after the script finishes).
- `crates/docs-gen/examples/`: template inputs for the docs pipeline
  itself (`generic-readme`, `resume`), not OxDock scripts.
