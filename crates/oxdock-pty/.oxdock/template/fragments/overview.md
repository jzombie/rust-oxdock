Shared pseudo-terminal pump. One blocking call spawns a child under a
pty sized from a shared cell, pumps caller bytes through the master,
follows window changes, drains the console to quiescence, and reports
the exit code. Local `WITH_IO [tty]` runs and the SSH plugin's remote
terminal sessions share this pump instead of reimplementing it.

The crate owns threads and OS handles but no run model: callers pass
plain byte sources and sinks plus a size source and get a blocking
call back. It never touches `oxdock-core`, `oxdock-process`, or any
pipe backend, keeping the sync core and its Miri story untouched.
Miri cannot allocate a real pty, so tests that spawn one live with
the callers under `ignore`; everything here is either pure or
constructor-only.
