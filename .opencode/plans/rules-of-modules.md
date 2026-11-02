# Rules of OxDock Modules

- Must not inherit their caller's context, ever
- Must be deterministic, regardless of where they are imported (must not ever inherit their caller's context)
- Must be able to maintain their own global state, which is shared between callers, via function calls (no "parallel global" state; a single counter function maintaining a global state should share between all callers)
- Must define their own WORKSPACE, else will ALWAYS default to SNAPSHOT, just like any other file
- Only export functions
- Can only import functions (and custom types)
