# oxdock-pipe

Anonymous pipe backends for OxDock: spillable script buffers, OS pairs, and owned handle slots.

> Part of the [OxDock](https://github.com/jzombie/rust-oxdock) workspace.

Anonymous pipe backends. Handles start unbound and materialize lazily on
first binding (script buffers by default, OS kernel pairs for pure
single-`RUN` pipelines); cloning a handle shares the backend, and the last
drop closes. Under Miri everything stays on script pipes.

### Layout

- `spill`: the spillable byte buffer. Bytes accumulate in memory and spill
  to a guarded temp file past the configured threshold, with a backlog cap
  bounding disk use. Thresholds ride on the value so tests construct
  small-threshold buffers explicitly; disk spilling is compiled out under
  Miri.
- `backend`: the script-pipe backend. One `PipeInner` holds the buffer plus
  writer/keeper accounting with blocking reads, timeout-bounded reads for
  bridge workers, and explicit close; `KeeperGuard` pins transient gaps for
  background tasks.
- `slot`: owned handles and OS pairs. A `PipeHandle` is a mutex cell over a
  `Slot` that starts `Unbound` and decides its kind once, under the cell
  lock, on first binding. OS pairs sit behind take-once slots: the first
  producer and consumer each take their half, and any further take bails
  instead of stealing the descriptor.

There is no central pipe index anywhere: resolution, keepers, peeks, and
diagnostics (`inspect`, `peek`, `script_backend`) all operate on handles
already in hand. `SharedInput` / `SharedOutput` aliases live here and are
re-exported from `oxdock-process`, keeping this crate a leaf
(`anyhow` + `oxdock-fs` only) so `oxdock-parser` pipe values can own
handles without a dependency cycle.

## License

`oxdock-pipe` is distributed under the terms of the [Apache License (Version 2.0)](https://github.com/jzombie/rust-oxdock/blob/main/LICENSE).
