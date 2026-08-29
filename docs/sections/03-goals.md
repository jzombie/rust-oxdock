## Goals

OxDock has a simple goal to provide a simple DSL that works the same across Mac, Linux, and Windows, including support for background processes, symlinks, and boolean conditionals (such as env and platform-based command filtering), which runs the same whether it's used as a preprocessing step in a build-time Rust macro, or as a CLI program, regardless of platform it is building on.

Every internal command is engineered to run the same way across platforms, except for the `RUN` command, which calls native programs.

**OxDock adds no additional runtime dependencies if used as a macro preprocessor.**
