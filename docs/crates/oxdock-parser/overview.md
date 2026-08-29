`oxdock-parser` builds the executable steps for the OxDock DSL that powers the
CLI and the embedding macros. The DSL is intentionally compact, but it now has
an explicit grammar so that other tooling (formatters, language servers, IDE
plugins, etc.) can understand scripts without re‑implementing the parser.
