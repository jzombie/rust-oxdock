| Command | Syntax |
| --- | --- |
| [`INHERIT_ENV`](#inherit_env) | `INHERIT_ENV [KEY1, KEY2, ...]` |
| [`WORKDIR`](#workdir) | `WORKDIR <path>` |
| [`WORKSPACE`](#workspace) | `WORKSPACE SNAPSHOT\|LOCAL` |
| [`ENV`](#env) | `ENV KEY=value` |
| [`ECHO`](#echo) | `ECHO <message>` |
| [`RUN`](#run) | `RUN <command...>` |
| [`RUN_BG`](#run_bg) | `RUN_BG <command...>` |
| [`COPY`](#copy) | `COPY [--from-current-workspace] <from> <to>` |
| [`WITH_IO`](#with_io) | `WITH_IO [bindings] [command \| { block }]` |
| [`COPY_GIT`](#copy_git) | `COPY_GIT [--include-dirty] <rev> <src> <dst>` |
| [`HASH_SHA256`](#hash_sha256) | `HASH_SHA256 <path>` |
| [`SYMLINK`](#symlink) | `SYMLINK <from> <to>` |
| [`MKDIR`](#mkdir) | `MKDIR <path>` |
| [`LS`](#ls) | `LS [<path>]` |
| [`CWD`](#cwd) | `CWD` |
| [`READ`](#read) | `READ [<path>]` |
| [`WRITE`](#write) | `WRITE <path> [<contents>]` |
| [`RAW_WRITE`](#raw_write) | `RAW_WRITE <path> <contents>` |
| [`APPEND`](#append) | `APPEND <path> [<contents>]` |
| [`EXPAND`](#expand) | `EXPAND [<path>] [<KEY=val> ...]` |
| [`ASSERT_FILE`](#assert_file) | `ASSERT_FILE [--hash <sha256>] <path> [<expected>]` |
| [`ASSERT_DIR`](#assert_dir) | `ASSERT_DIR <path>` |
| [`ASSERT_ABSENT`](#assert_absent) | `ASSERT_ABSENT <path>` |
| [`ASSERT_STDOUT`](#assert_stdout) | `ASSERT_STDOUT <substring>` |
| [`EXIT`](#exit) | `EXIT <code>` |
