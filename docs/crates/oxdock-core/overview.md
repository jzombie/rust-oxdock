# oxdock-core

`oxdock-core` is the small Dockerfile-inspired DSL used by [OxDock](../). It provides a compact DSL to script build-time actions (spawn processes, copy files, write files, create symlinks, etc.) and a tiny runtime API for parsing and executing those scripts from tests or other code.

This crate is not intended to be used on its own.
