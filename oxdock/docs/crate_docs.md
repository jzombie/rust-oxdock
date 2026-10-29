**OxDock is a Dockerfile-inspired DSL for native builds, multi-node pipelines, and sealed remote execution.**

Bash gives you strings and exit codes. OxDock gives you [typed values](https://github.com/jzombie/rust-oxdock#value-types), [guarded paths](https://github.com/jzombie/rust-oxdock#workspaces--filesystem), [managed pipes](https://github.com/jzombie/rust-oxdock#transport-pipes), and identical behavior on Linux/macOS/Windows (the things you'd re-implement badly in every growing bash script). And with hot-loading [MCP tooling](https://github.com/jzombie/rust-oxdock#mcp-hosting), *English-to-OxDock* just skips the boilerplate of getting there. [Workspaces](https://github.com/jzombie/rust-oxdock#workspaces--filesystem) (`SNAPSHOT`, `LOCAL`, `CACHE`, `SYSTEM`) and [scoped blocks](https://github.com/jzombie/rust-oxdock#guards-and-scoped-blocks) with [selective environment inheritance](https://github.com/jzombie/rust-oxdock#selective-environment-inheritance) give every run an isolated root that reverts on scope exit. When plain shell is the right tool, [`RUN`](https://github.com/jzombie/rust-oxdock#run) still reaches it, and [platform guards](https://github.com/jzombie/rust-oxdock#platform-guards) keep the OS-specific bits fenced: the two compose instead of competing.

Unlike Docker's Linux-VM requirement on non-Linux hosts, OxDock runs directly on Linux, macOS, and Windows. Execute scripts on bare metal, inside containers, or dispatched across remote machines simultaneously. No daemons. No hypervisors.

Platform gating selects OS-specific steps, and remote targets bind over any stdio transport. Only RUN touches the host shell.

The syntax is line-oriented with optional semicolon terminators and a static type system checking scripts before the first step runs. Plain Rust functions become script functions with one attribute: `#[oxdock_func]` exports them into namespaced modules scripts call as `DEMO::NAME(...)`. See [Extending OxDock from Rust](#extending-oxdock-from-rust).

[Documentation](https://docs.rs/oxdock/0.32.0-alpha/oxdock/)

## Dynamically embed assets at compile time

Embed build-time dependencies from any language: scripts run inline during `rustc`, and their artifacts ship inside the binary with zero heap allocation, `no_std` included:

```rust
use oxdock_macros::oxdock_embed;

oxdock_embed! {
    // Generated struct for resource lookups via `SiteAssets::get(path)`.
    name: SiteAssets,

    // Manifest-relative staging directory. Run `cargo build` locally to generate
    // the files and `.oxdock_hash` fingerprint, then commit `prebuilt/` to git.
    // `cargo publish` vendors these files so downstream crates embed them
    // directly without re-running the script on read-only registry paths.
    // Set OXDOCK_EMBED_FORCE_REBUILD=1 to force re-execution.
    out_dir: "prebuilt",

    script: {
        // Executes in an isolated temporary workspace (`WORKSPACE SNAPSHOT`),
        // keeping the source tree clean unless `WORKSPACE LOCAL` is requested.
        ENV PROJECT=OxDock
        MKDIR dist

        // Exec form spawns cargo portably without a shell. Asserting the version string
        // in memory verifies the toolchain without baking volatile output into staged assets.
        LET $cargo: STRING = RUN ["cargo", "--version"]
        ASSERT_CONTAINS $cargo "cargo"

        // Assets stage under subdirectories relative to `out_dir` (e.g., prebuilt/dist/os.txt)
        WRITE dist/hello.txt "hello from {{ env:PROJECT }}"
        WRITE dist/project.txt "the-project"
        WRITE dist/manifest.txt "the-manifest"
    }
}

fn main() {
    // Verify we can read the resources we just created
    let hello = SiteAssets::get("dist/hello.txt").expect("hello must be embedded");
    assert_eq!(hello.data.as_ref(), b"hello from OxDock");

    let project = SiteAssets::get("dist/project.txt").expect("project must be embedded");
    assert_eq!(project.data.as_ref(), b"the-project");

    let manifest = SiteAssets::get("dist/manifest.txt").expect("manifest must be embedded");
    assert_eq!(manifest.data.as_ref(), b"the-manifest");
}
```

> You can find an example of this workspace dogfooding this very design in its own CLI binary by searching for: [usage-prebuilt](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+usage-prebuilt&type=code).

For each artifact the macro emits a constant backed by `include_bytes!`, which bakes the file bytes into read-only binary data during compilation. At runtime `get()` scans a static table and returns a borrowed slice, so there are no file reads and no heap allocation. The support types only need `alloc::borrow::Cow` and core iterators, which is why it works in `no_std`.

Asset scripts resolve `STD` (via `IMPORT [STD]`) and `SCRIPT` functions only. There is no `modules:` prefix here, and that is structural, not missing: opaque modules defer membership to runtime, but asset scripts execute at compile time with no `Engine` to resolve against. Scripts needing host functions belong in `build.rs` through the `Engine` facade instead.

## Run scripts inline

The `oxdock!` macro builds the same DSL into a `Vec<Step>` at compile time, so tests and tools can run scripts without a file. Pass the steps to a `run_steps_*` runner with a guarded root. The root types live in `oxdock-fs`, so add both crates: `cargo add oxdock oxdock-fs`. Only portable commands are used below, so the script behaves identically on every OS.

```rust
use oxdock::{oxdock, oxdock_parser, run_steps_with_context};
use oxdock_fs::{GuardedPath, PathResolver};

// A version stamping pipeline: variables, a function, a loop over a list,
// a conditional call, templates, and native assertions. The version comes
// from Cargo at compile time, never a literal.
let crate_version = env!("CARGO_PKG_VERSION");
let steps: Vec<oxdock_parser::Step> = oxdock! {
    ENV PROJECT=OxDock
    LET $version: STRING = #crate_version
    MKDIR dist

    FUNC STAMP($name: STRING) {
        WRITE dist/{{ $name }}.txt {{ $name }} {{ env:PROJECT }} {{ $version }}
        RETURN $name
    }
    FOR $name: STRING IN ["alpha", "beta"] {
        STAMP($name)
    }

    FUNC PICK($flag: BOOL) {
        IF $flag {
            RETURN "alpha"
        }
        RETURN "beta"
    }
    LET $picked: STRING = PICK(true)
    WRITE dist/picked.txt {{ $picked }}

    LET $a: STRING = READ dist/alpha.txt
    LET $b: STRING = READ dist/beta.txt
    LET $p: STRING = READ dist/picked.txt
    ASSERT_EQ $a "alpha OxDock 0.32.0-alpha"
    ASSERT_EQ $b "beta OxDock 0.32.0-alpha"
    ASSERT_EQ $p "alpha"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/alpha.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "alpha OxDock 0.32.0-alpha"
);
```

Use `#var` to inject Rust values into the script (any value that implements `Display`). DSL variables keep their `$var` form and are unaffected. Guards accept injected values too, so Rust flags can gate steps. The script below wires a pipe between steps, reads one line back into a variable, and expands every generated file.

```rust
use oxdock::{oxdock, oxdock_parser, run_steps_with_context};
use oxdock_fs::{GuardedPath, PathResolver};

let project = "OxDock";
let verbose = true;
let steps: Vec<oxdock_parser::Step> = oxdock! {
    ENV PROJECT=#project
    MKDIR dist
    [bool:#verbose] WRITE dist/verbose.log "verbose on"

    LET $log: PIPE
    WITH_IO [stdout=$log] ECHO "built {{ env:PROJECT }}"
    WITH_IO [stdin=$log] READ_LINE $line
    WRITE dist/build.txt "{{ $line }}"

    IMPORT [STD]
    FOR $f: STRING IN GLOB("dist/*.txt") {
        EXPAND $f
    }

    ASSERT_CONTAINS stdout "built OxDock"
    LET $build: STRING = READ dist/build.txt
    LET $verbose: STRING = READ dist/verbose.log
    ASSERT_EQ $build "built OxDock"
    ASSERT_EQ $verbose "verbose on"
};

let temp = GuardedPath::tempdir().expect("tempdir");
let root = temp.as_guarded_path().clone();
run_steps_with_context(&root, &root, &steps).expect("run script");

let resolver = PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
let out = root.join("dist/build.txt").expect("out path");
assert_eq!(
    resolver.read_to_string(&out).expect("read out"),
    "built OxDock"
);
```

The top level runners need the default `cli` feature. With `--no-default-features`, run the same steps through `oxdock::oxdock_core::run_steps_*` instead.

Scripts that call host functions declare their modules up front: the macro parses at compile time with only `STD` known, so `modules: [DEMO],` as the first line makes `IMPORT [DEMO]` and `DEMO::...` calls resolve (membership is checked at runtime). Scripts using only `STD` and `SCRIPT` functions omit it. See [Extending OxDock from Rust](#extending-oxdock-from-rust) for the complete example.

## Extend the language

New types and functions take two attributes, a type registration, and a function module. Small
`Copy` scalars can ride inline on the stack with zero allocation
instead of heap boxing; both forms, with stateful functions, live under
[Extending OxDock from Rust](#extending-oxdock-from-rust) below.

```rust
use oxdock::{Engine, HostModule, OxDockFn, OxDockType, Value, oxdock_func, oxdock_type};
use oxdock::oxdock_core::TypeTag;
use std::fmt;

/// Word count summary: computed in Rust, carried as one script value.
#[oxdock_type(name = "STATS")]
#[derive(Debug, Clone, PartialEq)]
struct Stats {
    words: usize,
    chars: usize,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} words, {} chars", self.words, self.chars)
    }
}

/// Summarize a string: `STATS("hello brave world")` carries its counts.
#[oxdock_func(pure, name = "STATS")]
fn summarize(text: String) -> anyhow::Result<Value> {
    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    Ok(Value::mint_heap(
        Stats::descriptor(),
        Stats { words, chars },
    ))
}

/// Read the word count back out: `WORD_COUNT($s)` is an `INT`.
#[oxdock_func(pure, returns = TypeTag::Int)]
fn word_count(summary: Value) -> anyhow::Result<Value> {
    let Some(stats) = summary.read_heap::<Stats>(Stats::descriptor()) else {
        anyhow::bail!("WORD_COUNT() expects a STATS value");
    };
    Ok(Value::int(stats.words as i64))
}

fn main() -> anyhow::Result<()> {
    let mut engine = Engine::new();
    engine.register_type::<Stats>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![Summarize::registration(), WordCount::registration()],
        // Opaque custom types (`#[oxdock_type]` descriptors) this module's
        // functions mint or accept. Empty: the examples register types on
        // the engine directly, or use none.
        types: vec![],
        // Named MAP shapes for functions returning structured bundles
        // (field tables like `NET_LISTEN_INFO`). These return scalars and
        // opaque tags only, so none.
        record_schemas: vec![],
        // Shared state for function bodies, read with
        // `cx.module_state::<T>("MODULE")` and attached with
        // `.with_state(...)`. These are stateless, so none.
        states: vec![],
    });
    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let steps: Vec<oxdock::oxdock_parser::Step> = oxdock::oxdock! {
        modules: [DEMO],
        IMPORT [DEMO]
        LET $s: STATS = STATS("hello brave world")
        LET $n: INT = WORD_COUNT($s)
        ASSERT_EQ $n 3
    };
    let run = engine.run_steps(&root, &steps)?;
    assert_eq!(format!("{}", run.bindings["s"]), "3 words, 17 chars");
    Ok(())
}
```

## Runtime architecture

Four mechanisms keep script execution predictable: how values are stored, how bytes move between commands, how state stays isolated, and how the host stays sandboxed. Each one is shown running below.

### Values: words, not objects

Every script value is a fixed 128 bit word that lives on the stack: a static type descriptor pointer plus a 64 bit payload. Only payload contents may live on the heap, never the word itself.

Scalars that fit (`INT`, `FLOAT`, `BOOL`, handles) ride inline, copied into the payload byte for byte with zero allocation, so arithmetic and loop counters run at register speed and never touch the allocator. Strings and host types ride behind a thin pointer to a single owned box holding the concrete Rust type, while containers (`LIST`, `MAP`) ride behind a thin pointer to a shared reference counted buffer. The pointer stays thin because every payload type is sized, so there are no fat pointers and no trait objects anywhere in the path.

A naive tagged enum needs 32 bytes per value (a 24 byte payload plus tag and padding), so the word form holds four values per 64 byte cache line instead of two.

Type checks compare one descriptor address, and operations (`clone`, `drop`, equality, formatting) call the descriptor directly, with no registry lookup and no lock. Each type owns one compile time descriptor singleton, so identity is pointer equality that fails closed. Host types extend the same path: `#[oxdock_type]` derives a static descriptor for the payload struct, and `inline` selects the zero allocation form for small `Copy` scalars.

There is no garbage collector because values form trees, not graphs. Each exclusive heap word owns its box exactly once: cloning allocates a fresh box with a deep copy, dropping frees it. Each container word co-owns its buffer instead: cloning a `LIST` or `MAP` bumps a reference count in constant time with no allocation, dropping releases one count. A `LIST` owns its items and a `MAP` owns its entries. Mutating a shared container detaches first, so clones never observe each other's writes: assignment behaves as a copy.

Nothing is mutably borrowed from two places, so cycles cannot form and plain deterministic cleanup suffices. Pointer casts always round trip through the same concrete box or buffer type, and inline words never enter the pointer domain, which keeps provenance intact. The lifecycle is checked under Miri.

No pauses, no write barriers, no background collector. Python, JavaScript, and Lua permit aliasing and cycles and need tracing collectors to reclaim them; here there is nothing to trace.

| Design | Stack word | Scalar allocs | Cache density | Host extensibility |
| --- | --- | --- | --- | --- |
| Tagged Rust enum | 32 bytes | 0 | 2 values per line | Closed |
| Boxed trait objects | 16 byte fat pointer | 1 per scalar | Medium | Open |
| 16 byte word (this design) | 16 bytes | 0 | 4 values per line | Open, static descriptors |
| NaN boxing (LuaJIT, V8) | 8 bytes | 0 | 8 values per line | Constrained |

Two trade offs come with the form. Cloning a string still deep copies it, since only containers share buffers. And 16 bytes is roomier than 8 byte NaN boxing, which buys clean 64 bit integer and float storage plus safe abstraction boundaries without pointer masking.

### Transport: pipes

A command's standard streams can be rerouted through pipes declared with `LET $p: PIPE`, so producers and consumers connect without touching the terminal or temp files. Buffers stay in memory and spill to a guarded temp file past 8 MiB, and background single command tasks can promote a pipe to a zero copy OS kernel pair instead.

```oxdock
LET $log: PIPE
WITH_IO [stdout=$log] ECHO hello
WITH_IO [stdin=$log] READ_LINE $line
ASSERT_EQ $line "hello"
```

### Scope isolation

State mutations stay where the script puts them. Entering a braced block or a function call snapshots variables and settings, and exiting restores all of them, so nothing leaks outward. Background tasks fork the same way, so concurrent workers cannot observe each other's half finished mutations. Only pipes and filesystem effects cross these boundaries, by design.

```oxdock
# Calls snapshot caller state: the parameter shadows without clobbering.
LET $v: STRING = "outer"
FUNC SHADOW($v: STRING) {
    LET $inner: STRING = "inner"
    RETURN $v
}

LET $out: STRING = SHADOW("param")
ASSERT_EQ $out "param"
ASSERT_EQ $v "outer"

# Background tasks fork the same way: worker mutations never escape.
LET $w: STRING = "outer"
LET $t: HANDLE = ASYNC {
    $w = "inner"
    ECHO "task-ran"
}
AWAIT $t
ASSERT_EQ $w "outer"
ASSERT_CONTAINS stdout "task-ran"
```

### Sandboxing

Every path resolves inside a guarded workspace root, and escapes are rejected before any filesystem call. Scripts start with only builtin keys (Cargo feature/cfg entries, `WORKSPACE_GIT_COMMIT`) and opt into host variables explicitly.

```oxdock expect_error:"escapes allowed root"
WRITE ../escape.txt "nope"
```

## Extending OxDock from Rust

Scripts call host functions and custom types that Rust code registers
under a module. Registration is a type plus a module on one facade, and
scripts name the module: `IMPORT [DEMO]` lets the rest call
`MAKE_TAG()` bare, or qualify as `DEMO::MAKE_TAG()`. This example runs as
written: the shape first, the definitions it names right below it.

### Complete example: define a type, define functions, run a script

```rust
use oxdock::{Engine, HostModule, OxDockFn, OxDockType, oxdock_func, oxdock_type};
use oxdock::oxdock_core::TypeTag;
use std::fmt;

// The script below is the DSL itself, not a string: the `oxdock!` macro
// builds it into steps at compile time.

fn main() -> anyhow::Result<()> {
    let mut engine = Engine::new();
    engine.register_type::<Tag>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![MakeTag::registration(), ReadTag::registration()],
        // Opaque custom types (`#[oxdock_type]` descriptors) this module's
        // functions mint or accept. Empty: the examples register types on
        // the engine directly, or use none.
        types: vec![],
        // Named MAP shapes for functions returning structured bundles
        // (field tables like `NET_LISTEN_INFO`). These return scalars and
        // opaque tags only, so none.
        record_schemas: vec![],
        // Shared state for function bodies, read with
        // `cx.module_state::<T>("MODULE")` and attached with
        // `.with_state(...)`. These are stateless, so none.
        states: vec![],
    });

    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root_path = temp.as_guarded_path().clone();
    let steps: Vec<oxdock::oxdock_parser::Step> = oxdock::oxdock! {
        modules: [DEMO],
        IMPORT [DEMO]
        LET $t: TAG = MAKE_TAG()
        LET $s: STRING = READ_TAG($t)
        ASSERT_EQ $s "demo"
        WRITE tag.txt "{{ $s }}::{{ $t }}"
    };
    let run = engine.run_steps(&root_path, &steps)?;
    assert!(run.bindings.contains_key("s"));

    let reader = oxdock_fs::PathResolver::new(root_path.root(), root_path.root()).unwrap();
    let written = reader
        .read_to_string(&root_path.join("tag.txt").unwrap())
        .expect("script writes tag.txt");
    assert_eq!(written.trim(), "demo::tag:demo");
    Ok(())
}

// The type. `#[oxdock_type]` implements `OxDockType` on the payload struct
// itself. The default is the heap path: the word holds a thin pointer to
// an owned box. Heap payloads require `Clone + PartialEq + Display +
// Debug + Send + Sync`.

/// Opaque label type.
#[oxdock_type(name = "TAG")]
#[derive(Debug, Clone, PartialEq)]
struct Tag(String);

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "tag:{}", self.0)
    }
}

// The functions. `#[oxdock_func(pure)]` implements `OxDockFn` on a
// registration marker named after the function (`make_tag` becomes
// `MakeTag`). The DSL name defaults to the uppercased Rust name.
// `Value::mint_heap` with the payload type's own descriptor mints a word
// of the registered type, and `read_heap` with that descriptor reads it
// back. No name lookup runs anywhere on this path.

/// Mint one opaque label.
#[oxdock_func(pure)]
fn make_tag() -> anyhow::Result<oxdock::Value> {
    Ok(oxdock::Value::mint_heap(Tag::descriptor(), Tag("demo".into())))
}

/// Read the payload back out through a descriptor-checked typed read.
#[oxdock_func(pure, returns = TypeTag::String)]
fn read_tag(val: oxdock::Value) -> anyhow::Result<oxdock::Value> {
    let Some(tag) = val.read_heap::<Tag>(Tag::descriptor()) else {
        anyhow::bail!("READ_TAG() expects a TAG value");
    };
    Ok(oxdock::Value::string(tag.0.clone()))
}
```

The shape is always the same. `LET $t: TAG = MAKE_TAG()` calls a host
function like any native one: arity, depth budget, and the declared `TAG`
coercion apply uniformly. `READ_TAG($t)` reclaims the payload through an
id checked read. `{{ $t }}` renders the custom value through its
`Display`, so interpolation, `WRITE`, and equality treat host values like
native ones. `TYPES()` lists every registered name and
`TYPE_DESCRIBE("TAG")` returns its summary and docs, so scripts introspect
host surface exactly like native surface. Script `TYPE` aliases ride the
same listing: `TYPES()` includes alias names and `TYPE_DESCRIBE("PERSON")`
reports the canonical target spelling. Host types stay opaque:
literal syntax, `$var.key` traversal, and `FOR` iteration remain `LIST`
and `MAP` only, so queryable containers expose host accessor functions
(`MATRIX_GET($m, $row, $col)`) instead of new syntax.

### Reference: stateful functions

Without `pure`, the first parameter must be `cx: &mut StepCtx<P>`, which
exposes variables, environment, pipes, and IO. Override the DSL name and
the declared return type explicitly when the defaults do not fit:

```rust
use oxdock::{HostModule, OxDockFn, StepCtx, oxdock_func};
use oxdock::oxdock_core::TypeTag;
use oxdock::oxdock_core::ProcessManager;

/// Read an environment variable, defaulting to empty.
#[oxdock_func(name = "ENV_OR", returns = TypeTag::String)]
fn env_or<P: ProcessManager>(
    cx: &mut StepCtx<P>,
    key: String,
) -> anyhow::Result<oxdock::Value> {
    Ok(oxdock::Value::string(cx.get_env(&key).unwrap_or_default()))
}

fn main() {
    let mut engine = oxdock::Engine::new();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![EnvOr::registration()],
        // Opaque custom types (`#[oxdock_type]` descriptors) this module's
        // functions mint or accept. Empty: the examples register types on
        // the engine directly, or use none.
        types: vec![],
        // Named MAP shapes for functions returning structured bundles
        // (field tables like `NET_LISTEN_INFO`). These return scalars and
        // opaque tags only, so none.
        record_schemas: vec![],
        // Shared state for function bodies, read with
        // `cx.module_state::<T>("MODULE")` and attached with
        // `.with_state(...)`. These are stateless, so none.
        states: vec![],
    });
}
```

Parameters accept `Value`, `String`, `i64`, `f64`, and `bool`, in either
form. Stateful markers register exactly like pure ones. Pure functions run
on the compiled math path too; stateful ones stay on the AST path unless
they opt in with `#[oxdock_func(rpn)]` (as `GLOB` does).

### Reference: inline types

`#[oxdock_type(inline)]` selects the zero allocation path for `Copy`
scalars that fit in 64 bits: the word holds the value bytes directly. This
is the same derivation the startup integer, float, boolean, and handle
types use:

```rust
use oxdock::{OxDockType, oxdock_type};
use std::fmt;

/// Entity handle.
#[oxdock_type(name = "ENTITY", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct EntityId(u64);

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "entity#{}", self.0)
    }
}

fn main() {
    let mut engine = oxdock::Engine::new();
    engine.register_type::<EntityId>();
    let val = oxdock::Value::mint_inline(EntityId::descriptor(), EntityId(7));
    assert_eq!(val.inline_bits(), 7);
    assert_eq!(format!("{val}"), "entity#7");
    assert_eq!(val.clone(), val);
}
```

Four limits decide what can ride inline:

1. **Fixed 64 bit slot.** Anything larger panics at mint time (a runtime
   check, not a compile time one).
2. **`Copy` payloads only.** Inline clones copy bits and drops do
   nothing, which resource owning types cannot satisfy.
3. **No variable length data.** `STRING`, `LIST`, and `MAP` can never fit
   a fixed slot and stay behind the pointer.
4. **Widening punishes everything.** A bigger slot means fewer words per
   cache line and costlier moves for the scalars that dominate scripts.

One boundary to keep straight: words stored inside a `LIST` or `MAP`
live in that container's heap buffer, so inline describes the payload
encoding, not a promise that every word sits on the stack.

### Reference: queryable containers

Opaque types can still answer queries: pair the payload with host
accessor functions and scripts read cells, lengths, and keys through
ordinary calls, with no new syntax. The example below mints a grid and
reads one cell through its accessor; from a script the same call spells
`LET $c: INT = MATRIX_GET($m, 0, 1)`. Key paths and `FOR` iteration stay
unavailable by the boundary above.

```rust
use oxdock::{HostModule, OxDockFn, OxDockType, Value, oxdock_func, oxdock_type};
use oxdock::oxdock_core::TypeTag;
use std::fmt;

/// Integer grid with no literal syntax: scripts query it through functions.
#[oxdock_type(name = "MATRIX")]
#[derive(Debug, Clone, PartialEq)]
struct Matrix(Vec<Vec<i64>>);

impl fmt::Display for Matrix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "matrix[{}x{}]", self.0.len(), self.0.first().map_or(0, Vec::len))
    }
}

/// Mint a fixed 1x2 grid.
#[oxdock_func(pure)]
fn make_matrix() -> anyhow::Result<Value> {
    Ok(Value::mint_heap(
        Matrix::descriptor(),
        Matrix(vec![vec![1, 2]]),
    ))
}

/// Read one cell by row and column.
#[oxdock_func(pure, returns = TypeTag::Int)]
fn matrix_get(board: Value, row: i64, col: i64) -> anyhow::Result<Value> {
    let Some(grid) = board.read_heap::<Matrix>(Matrix::descriptor()) else {
        anyhow::bail!("MATRIX_GET() expects a MATRIX value");
    };
    let cell = usize::try_from(row)
        .ok()
        .and_then(|r| grid.0.get(r))
        .and_then(|r| usize::try_from(col).ok().and_then(|c| r.get(c)))
        .copied()
        .ok_or_else(|| anyhow::anyhow!("index out of bounds"))?;
    Ok(Value::int(cell))
}

fn main() -> anyhow::Result<()> {
    let word = Value::mint_heap(Matrix::descriptor(), Matrix(vec![vec![1, 2]]));
    let cell = matrix_get(word, 0, 1).unwrap();
    assert_eq!(cell.as_i64(), Some(2));

    let mut engine = oxdock::Engine::new();
    engine.register_type::<Matrix>();
    engine.register_module(HostModule {
        name: "DEMO".to_string(),
        funcs: vec![
            MakeMatrix::registration(),
            MatrixGet::registration(),
        ],
        // Opaque custom types (`#[oxdock_type]` descriptors) this module's
        // functions mint or accept. Empty: the examples register types on
        // the engine directly, or use none.
        types: vec![],
        // Named MAP shapes for functions returning structured bundles
        // (field tables like `NET_LISTEN_INFO`). These return scalars and
        // opaque tags only, so none.
        record_schemas: vec![],
        // Shared state for function bodies, read with
        // `cx.module_state::<T>("MODULE")` and attached with
        // `.with_state(...)`. These are stateless, so none.
        states: vec![],
    });
    let temp = oxdock_fs::GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    let run = engine.run_script(
        &root,
        "IMPORT [DEMO]\nLET $m: MATRIX = MAKE_MATRIX()\nLET $c: INT = MATRIX_GET($m, 0, 1)\n",
    )?;
    assert_eq!(run.bindings["c"].as_i64(), Some(2));
    Ok(())
}
```

### Reference: evaluation paths and the `rpn` flag

Two evaluators run scripts, and the distinction decides where a host
function may run. The AST evaluator walks the parsed tree one step at a
time. Every step knows its line number, so failures name it (`step 1:
INT() expects 1 argument(s), got 2`). Scopes, pipes, declarations, and
every statement form live here. The RPN evaluator runs arithmetic and
comparison expressions compiled to a flat stack-machine program
(`PushConst`, `LoadVar`, `Call`, `Add`, ...). A stack program carries
values only: no statements, no scopes, no pipes, and no step numbers, so
a failing call inside math reports the bare error (`INT() expects 1
argument(s), got 2`).

The tradeoff is expressiveness against compactness, not speed. The tree
can say anything the language can say, with errors that point at the
script. The stack program can only compute values, which is exactly what
math needs and nothing more. That is why not everything runs on RPN:
statements, declarations, scoping, and IO orchestration have no stack
encoding, and step-numbered errors require the tree.

For host functions the rule follows from that split. Pure functions take
only values and touch nothing, so they run on both paths with no flag.
Stateful functions default to AST-only. Opt in with
`#[oxdock_func(rpn)]` only for read-only queries that stay meaningful
inside math (`GLOB`, `LOAD_TOML`, `LOAD_JSON` do this): the function
still receives full step context, but its failures lose their step
numbers. Side-effecting stateful functions stay out, since stack-order
execution with step-less errors is the worst place for an effect to go
wrong.

### Why embedding Rust here stays simple

Compared against embedding Rust in Python, there are three structural
reasons the host boundary stays small, and none of them are API polish.

#### No foreign runtime to host

Embedding Rust in Python means linking an interpreter, managing the
GIL, and marshaling across two object models with different lifetimes.
OxDock functions are plain Rust functions returning `Result<Value>`:
the VM is just the calling convention, and values are fixed size words
with deterministic lifetimes, so there is nothing to pin, nothing
reference counted, and nothing kept alive across the boundary.

#### No binding layer

The Python path needs module registration plus type conversions
negotiated with dynamic types. `#[oxdock_func]` derives the
registration marker, the arity gate, and the `String`, `i64`, `f64`,
`bool`, and `Value` extraction from the signature, and doc comments
become introspectable metadata for free.

#### No build system dance

No maturin, no ABI tags, and no wheels built per interpreter. The host
crate depends on `oxdock-core` and calls `Engine::register_module`.
The one thing Python still wins is its C ABI as a stable interop target
for other languages. The OxDock boundary is Rust only, which is exactly
what keeps it cheap.

## Machine model

How the machinery operates, for the curious. Behavior contracts live in
the command and function reference. This section maps those contracts
onto the moving parts: words, evaluators, and the stack program that
runs math.

### Words

Every script value is one 128 bit word: a descriptor pointer plus a 64
bit payload. The descriptor names the type and carries the vtable that
clones, drops, compares, and renders the value. The payload is either
inline bytes (for `Copy` scalars that fit in 64 bits, with zero
allocation) or a pointer to one owned heap box (for containers and
everything else). Words are fixed size and deterministic to move, which
is what keeps the host boundary cheap: plain Rust functions returning
`Result<Value>`, nothing to pin and nothing reference counted. The
glossary defines each of these terms precisely.

### Two evaluators

Scripts run under two evaluators. The AST evaluator walks the parsed
tree one step at a time, with line numbers on every failure. The RPN
evaluator runs arithmetic and comparison as a flat stack program with
values only and no step numbers. Which functions may run where is the
[Contexts](#contexts) rule: everything runs on the tree, and only pure
functions plus explicit `rpn` opt ins run on the stack program.

### The stack program

Parsing lowers each arithmetic or comparison subtree in three passes.
First, literal only subtrees fold to their value and vanish. Then the
survivors flatten to postfix ops with resolved arities: loads for
variables, env reads, and keypaths; prebuilt constants; calls; and one
op per arithmetic or comparison operator. Anything without a stack
encoding (`!`, `&&` and `||`, list and map literals, pipes) stays a
tree node and runs on the AST path. The static pre-pass type checks the
same program abstractly before the first step runs, so math shape errors
surface early with the same step free wording the runtime reports. The
program is built once per parse and runs again on every evaluation, so a
`WHILE` condition pays lowering once no matter how many iterations it
runs.

The demos below are computed, not copied: each placeholder lowers its
expression through the real parser on every docs run, so the shown ops
cannot drift from the implementation.

`$a + 2 * $b` is the classic shape: loads and one constant feeding two
operators.

```text
`$a + 2 * $b` lowers to 5 ops:
1. LoadVar($a)
2. PushConst(2)
3. LoadVar($b)
4. Mul
5. Add
```

`1 + 2 * 3` never reaches the stack machine: both subtrees fold at parse
time.

```text
`1 + 2 * 3` folds to the literal `7` at parse time: no ops are emitted
```

`!$ready` has no stack encoding and stays on the tree path.

```text
`!$ready` has no RPN encoding and stays a tree node: `!$ready`
```

# DSL Reference

> **Prototype status**: OxDock is still being prototyped. DSL syntax and Rust APIs may change without deprecation warnings until the first stable release.

Scripts are sequences of instructions, one per line (`;` separates two; quoted separators stay literal). Instructions may be prefixed with **guards** (`[...]`) that decide whether they run, and grouped into **scoped blocks** (`{ ... }`). The authoritative grammar is [`crates/oxdock-parser/src/dsl.pest`](https://github.com/jzombie/rust-oxdock/blob/main/crates/oxdock-parser/src/dsl.pest), which is also embedded in the parser crate as the `LANGUAGE_SPEC` constant for tooling.

## Lexical structure

- **Commands are uppercase** and case-sensitive: `WORKDIR`, not `workdir`. Lowercase or mixed-case spellings are parse errors with an uppercase hint.
- One instruction per line; a semicolon (`;`) splits multiple instructions on a single line.
- Paths and arguments use forward slashes (`/`) for portability (see [Path Separators](#path-separators)).
- Scripts start with host-mirrored essentials (`PATH`, `HOME`, temp bindings, loader anchors); anything beyond those needs `INHERIT_ENV` to opt specific keys in (see [Selective environment inheritance](#selective-environment-inheritance)).

### Declared variable types

Every variable binding declares its type at the binding site. `LET $name: TYPE = ...` creates the binding, `$name = ...` mutates it, and bodies use the bare `$name` reference. The leading `$` keeps mutation distinct from `KEY=value` command assignments. Repeating `LET` for the same name in the same scope is a redeclaration error. Loop variables are declared the same way: `FOR $item: STRING IN ...`. Valid types: scalars (`STRING`, `INT`, `FLOAT`, `BOOL`, `DURATION`, `PATH`), handles (`PIPE`, `HANDLE`, `SEMAPHORE`, `PERMIT`), `ANY`, and composed shapes (`LIST<STRING>`, `MAP<name: STRING, age: INT>`, arbitrary nesting, spacing insignificant). Bare `LIST` and `MAP` are runtime words, never declarations: unknown collections declare `LIST<ANY>` and `MAP<ANY>`. A field with a `?` suffix is optional (`MAP<held: BOOL, permit?: PERMIT>`): missing optional keys pass, missing required keys fail. `TYPE NAME = <shape>` binds one immutable name per run that every declaration form accepts.

```oxdock
LET $count: INT = 1
$count = 2
LET $msg: STRING = hello

FOR $item: STRING IN ["a", "b"] {
    ECHO "{{ $item }}"
}

WRITE count.txt "{{ $count }}"
LET $c: STRING = READ count.txt
ASSERT_EQ $c "2"
```

Shapes enforce at the binding: a bad element fails naming its position.

```oxdock
# A list of unknown maps and an inline record bind shaped values.
LET $ms: LIST<MAP<ANY>> = [{a: 1}]
LET $p: MAP<name: STRING, age: INT> = {name: "x", age: 3}
ASSERT_EQ $p.name "x"

# TYPE binds one reusable name; uses enforce structurally.
TYPE PERSON = MAP<name: STRING, age: INT>
LET $q: PERSON = {name: "y", age: 4}
LET $team: LIST<PERSON> = [$q]
LET $n: STRING = $team.0.name
ASSERT_EQ $n "y"
```

The `env:KEY` expression reads the script environment into a plain value. A `$var` reference never reads the environment, even when the names match:

```oxdock
ENV FOO="bar"
LET $e: STRING = env:FOO
WRITE env.txt "{{ $e }}"
LET $v: STRING = READ env.txt
ASSERT_EQ $v "bar"
```

### Functions and commands

Parentheses mark the boundary between computing a value and running a pipeline step. Builtin functions (`INSPECT`, `LOAD_TOML`, `LOAD_JSON`, `GLOB`, `INT`, `FLOAT`, `PATH_TYPE`) evaluate to an in-memory value and never write to standard output. They compute or query (`Value::Map`, `Value::String`, `Value::Int`, `Value::Float`, `Value::List`) with zero stream side effects, so they appear only where values are expected: on the right-hand side of `LET`, inside `IF` conditions, or nested in other calls. Commands (`READ`, `ECHO`, `RUN`, `WRITE`, `ASSERT_EQ`) are line-starting statements with space-separated arguments. They drive the I/O pipeline, streaming bytes to stdout or mutating state, which makes their output available to pipes and `LET` capture. When a function evaluates, process stdout stays completely untouched. When a command runs, streaming bytes is the payload:

```oxdock
// Functions compute values; stdout stays untouched.
IMPORT [STD]
LET $t: STRING = PATH_TYPE("missing.txt")
ASSERT_EQ $t "absent"

LET $n: INT = INT("41") + 1
ASSERT_EQ $n 42
ASSERT_EQ stdout ""
```

### Statements and semicolons

```oxdock
// One line, two instructions: the semicolon splits them.
ECHO one; ECHO two
ASSERT_CONTAINS stdout "one"
ASSERT_CONTAINS stdout "two"
```

### RUN shell and exec forms

Shell form (`RUN <command...>`) joins its arguments and runs the string in the system shell. Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly with no shell. Exec form has no shell expansion, globbing, redirection, or pipes. Quoted `{{ ... }}` templates still interpolate per element, and guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`) apply to both forms.

```oxdock
RUN ["cargo", "--version"]
ASSERT_CONTAINS stdout "cargo"
```

### Comments

Three comment styles are supported: `//` line comments, nestable `/* ... */` block comments, and `#` comments. A `#` comment occupies a whole line (optionally indented) and may also trail values inside multi-line `()`, `[]`, and `{}` brackets; inside a command payload a `#` is ordinary text. Similarly, `//` ends a `RUN` argument list but survives inside quoted strings:

```oxdock
// Slash comment at end of line.
# Hash comment occupies the whole line.

/* block comments
   /* nest */
   like this */
ECHO visible-after-comments
ASSERT_CONTAINS stdout "visible-after-comments"
```

```oxdock
ECHO hash-mid-line # stays-in-payload
ASSERT_CONTAINS stdout "hash-mid-line # stays-in-payload"

RUN echo run-args-stop-at-slashes // removed-as-comment
ASSERT_CONTAINS stdout "run-args-stop-at-slashes"

RUN echo "quoted // kept"
ASSERT_CONTAINS stdout "quoted // kept"
```

Comment markers inside quoted strings are always preserved.

### Command flags

Commands that take flags accept the value either after a space or joined with `=`: `--from-workspace LOCAL` and `--from-workspace=LOCAL` mean the same thing. Boolean flags take no value. Quoted arguments are never treated as flags, even when they start with `--`.

```oxdock roots:unified
WRITE flag-src.txt flag-content
COPY --from-workspace=LOCAL flag-src.txt flag-copy.txt
LET $body: STRING = READ flag-copy.txt
ASSERT_EQ $body "flag-content"
```

### Quoting and escaping

Arguments accept single- or double-quoted strings; the escape sequences `\"` and `\'` embed a quote, and any other backslash escape keeps the escaped character while dropping the backslash. Quoted fragments containing whitespace, `;`, newlines, `//`, or `/*` retain their quotes when `RUN` reconstructs the command string:

```oxdock
// Single and double quotes behave identically.
ECHO 'single quotes'
ASSERT_CONTAINS stdout "single quotes"
ECHO "double quotes"
ASSERT_CONTAINS stdout "double quotes"

// \" embeds a quote; the backslash itself is consumed.
ECHO "escaped \" quote"
ASSERT_CONTAINS stdout 'escaped " quote'

// Quoted separators stay literal: no instruction splits here.
ECHO 'semi;colon'
ASSERT_CONTAINS stdout "semi;colon"
```

## Guards and scoped blocks

A guard is a bracketed expression that gates the instruction or block that follows it. Inside the brackets:

- Guards are namespaced `key:value` pairs: `os:macos`, `os:linux`, `os:windows`, `arch:x86_64` (any documented target-architecture value), `bool:true`, `env:KEY`, `eq(env:KEY, value)`. `family:unix` and `family:windows` are accepted aliases for `any(os:macos, os:linux)` and `os:windows` respectively, lowered at parse so every platform check runs under `os:`. Matching is case-sensitive throughout.
- A comma-separated list means **AND**: `[env:A, os:linux]`.
- Disjunction is expressed as a call: `any(expr, expr, ...)` with at least two branches, not an infix operator.
- Conjunction is expressed as a call (`all(expr, expr, ...)`) or implicitly via comma separation.
- Any predicate may be negated with `not(...)`: `[not(env:SKIP)]`.
- Parentheses group expressions: `[any(env:A, os:linux), os:macos]`.

Guards attach to the next instruction. Several guard lines in a row chain onto the same target, and a guard immediately followed by `{` opens a guarded block whose guard applies to every enclosed instruction.

Guard evaluation checks the script environment only: `ENV` entries, `INHERIT_ENV` opt-ins, and builtin keys. Non-essential host variables stay invisible unless inherited, so guards interact naturally with `INHERIT_ENV` and `ENV`.

### Environment guards

```oxdock env:DEPLOY_TARGET=staging
// Copy the key from the host environment (the runner injects it).
INHERIT_ENV [DEPLOY_TARGET]

// Passes when the variable exists with any non-empty value.
[env:DEPLOY_TARGET] ECHO deploy-target-visible
ASSERT_CONTAINS stdout "deploy-target-visible"

// Equality against the inherited value.
[eq(env:DEPLOY_TARGET, staging)] ECHO deploying-to-staging
ASSERT_CONTAINS stdout "deploying-to-staging"

// Inequality: skipped below, because DEPLOY_TARGET IS staging.
[ne(env:DEPLOY_TARGET, staging)] ECHO deploying-elsewhere
```

### Platform guards

```oxdock
// Exactly one block runs depending on the host OS; every command
// inside a guarded block inherits the block's guard. A shared
// counter incremented in each block proves exactly one ran.
LET $ran: INT = 0

[family:windows] {
  WRITE os-report.txt windows
  ECHO windows-detected
  LET $rep: STRING = READ os-report.txt
  ASSERT_EQ $rep "windows"
  ASSERT_CONTAINS stdout "windows-detected"
  $ran = $ran + 1
}

[family:unix] {
  WRITE os-report.txt unix-family
  ECHO unix-detected
  LET $rep: STRING = READ os-report.txt
  ASSERT_EQ $rep "unix-family"
  ASSERT_CONTAINS stdout "unix-detected"
  $ran = $ran + 1
}

ASSERT_EQ $ran 1
```

### Architecture guards

```oxdock
# Gate blocks on the host target architecture. Exactly one block
# runs on any host, and each block proves its own execution.
[arch:x86_64] {
  WRITE arch-report.txt x86_64
  ECHO arch-detected
  LET $rep: STRING = READ arch-report.txt
  ASSERT_EQ $rep "x86_64"
  ASSERT_CONTAINS stdout "arch-detected"
}

[arch:aarch64] {
  WRITE arch-report.txt aarch64
  ECHO arch-detected
  LET $rep: STRING = READ arch-report.txt
  ASSERT_EQ $rep "aarch64"
  ASSERT_CONTAINS stdout "arch-detected"
}

[not(any(arch:x86_64, arch:aarch64))] {
  WRITE arch-report.txt other-arch
  ECHO arch-detected
  LET $rep: STRING = READ arch-report.txt
  ASSERT_EQ $rep "other-arch"
  ASSERT_CONTAINS stdout "arch-detected"
}

# One block always runs, so this holds on every host.
ASSERT_CONTAINS stdout "arch-detected"
```

### Negation, disjunction, and composition

```oxdock env:OXDOCK_DOC_FEATURE_A=enabled
// Bring the runner-injected value into the script environment.
INHERIT_ENV [OXDOCK_DOC_FEATURE_A]

// not(...) inverts the predicate: passes because the variable does NOT exist.
[not(env:OXDOCK_DOC_UNDEFINED_VAR)] ECHO negation-passes-for-undefined
ASSERT_CONTAINS stdout "negation-passes-for-undefined"

// any(...) passes when ANY branch holds; A exists, so this runs.
[any(env:OXDOCK_DOC_FEATURE_A, env:OXDOCK_DOC_FEATURE_B)] ECHO or-matched-a-branch
ASSERT_CONTAINS stdout "or-matched-a-branch"

// Comma composes with AND: (A or linux) AND A: true here on every OS.
[any(env:OXDOCK_DOC_FEATURE_A, os:linux), env:OXDOCK_DOC_FEATURE_A] ECHO composed-and-or-guard
ASSERT_CONTAINS stdout "composed-and-or-guard"
```

### Multi-line guards

Bracket expressions may span lines. Chained guard lines apply conjunctively to the next instruction; here neither variable is defined, so the gated instruction is skipped:

```oxdock
IMPORT [STD]

// Brackets may span lines; chained lines AND together and gate
// the next command.
[
  env:OXDOCK_DOC_CHAIN_ONE,
  env:OXDOCK_DOC_CHAIN_TWO
]

// Neither variable exists, so this WRITE is skipped entirely.
WRITE chained.txt applied

// The artifact was never created.
LET $t: STRING = PATH_TYPE("chained.txt")
ASSERT_EQ $t "absent"
```

### Scoped blocks

Braced blocks scope everything: `LET` variables, `ENV` values, `WORKDIR`, and `WORKSPACE` all revert when the block exits. Files created inside a block persist on disk, and pipes registered with `WITH_IO` stay open. Those are the only things that cross a scope boundary. (A bare `{ ... }` needs an always-true guard: `[bool:true]`. Single commands, including single `WITH_IO` lines like `READ_LINE`, never open a scope.)

```oxdock
LET $a: STRING = "some_value"
ENV MODE="production"
MKDIR scoped_area
WORKDIR scoped_area

// Guarded block: LET, ENV, and WORKDIR below are scoped and revert
// when the block closes.
[bool:true] {
    LET $a: STRING = "inner_value"
    ENV MODE="staging"
    WRITE inner.txt "{{ $a }}-{{ env:MODE }}"
}

// $a is back to "some_value", MODE is back to "production",
// and cwd is back at scoped_area. Files persist.
LET $in_body: STRING = READ inner.txt
ASSERT_EQ $in_body "inner_value-staging"
WRITE outer.txt "{{ $a }}-{{ env:MODE }}"
LET $out_body: STRING = READ outer.txt
ASSERT_EQ $out_body "some_value-production"
```

Pipes cross scope boundaries the same way files do: a handle bound
inside stays usable after the block exits.

```oxdock
LET $p: PIPE
[bool:true] {
    WITH_IO [stdout=$p] ECHO "piped-out"
}
WITH_IO [stdin=$p] READ_LINE $line
ASSERT_EQ $line "piped-out"
```

`IF`/`ELSE` branches, `FOR` loop bodies, `TIMEOUT` bodies, `ASYNC` bodies, and `WITH_IO [..] { ... }` blocks are all scopes under the same rule: only files and pipes leak out.

### EXIT in nested blocks

`EXIT <code>` stops the pipeline immediately with an `EXIT requested with code <code>` error. Steps after it never run, at any nesting depth. Files written before the `EXIT` persist, which the fence below asserts; state unwinding and task teardown follow the same scope rules as everywhere else.

```oxdock expect_error:"EXIT requested with code 3"
WRITE before.txt "persisted"
LET $b: STRING = READ before.txt
ASSERT_EQ $b "persisted"

[bool:true] {
    EXIT 3
    WRITE unreachable.txt "never"
}
```

<!-- GENERATED by docs-gen from oxdock-parser metadata. Do not edit by hand. -->
## Command Reference

See the [changelog](https://github.com/jzombie/rust-oxdock/blob/main/CHANGELOG.md) for what changed in each release.

| Command | Syntax |
| --- | --- |
| [`WORKDIR`](#workdir) | `WORKDIR <path>` |
| [`WORKSPACE`](#workspace) | `WORKSPACE (SNAPSHOT\|LOCAL\|CACHE\|SYSTEM) [--local]` |
| [`ENV`](#env) | `ENV KEY=value` |
| [`INHERIT_ENV`](#inherit_env) | `INHERIT_ENV [<key>, ...]` |
| [`ECHO`](#echo) | `ECHO <message>` |
| [`RUN`](#run) | `RUN <command...> \| RUN ["exe", "arg", ...]` |
| [`COPY`](#copy) | `COPY [--from-workspace SNAPSHOT\|LOCAL\|CACHE\|SYSTEM] [--from-host \| --to-host] <from> <to>` |
| [`COPY_GIT`](#copy_git) | `COPY_GIT [--include-dirty] <rev> <src> <dst>` |
| [`SYMLINK`](#symlink) | `SYMLINK [--from-workspace SNAPSHOT\|LOCAL\|CACHE\|SYSTEM] <from> <to>` |
| [`MKDIR`](#mkdir) | `MKDIR <path>` |
| [`INSTALL`](#install) | `INSTALL <source> <dest>` |
| [`ARCHIVE`](#archive) | `ARCHIVE [--raw] <src> <dst>` |
| [`UNARCHIVE`](#unarchive) | `UNARCHIVE <src> <dst> [--max-bytes <bytes>]` |
| [`LS`](#ls) | `LS [<path>]` |
| [`CWD`](#cwd) | `CWD` |
| [`READ`](#read) | `READ [<path>]` |
| [`READ_LINE`](#read_line) | `READ_LINE $var` |
| [`PROMPT`](#prompt) | `PROMPT $var <message> [<default>]` |
| [`WRITE`](#write) | `WRITE <path> [<contents>]` |
| [`APPEND`](#append) | `APPEND <path> [<contents>]` |
| [`REMOVE`](#remove) | `REMOVE [--recursive] <path>` |
| [`EXPAND`](#expand) | `EXPAND [<path>] [<KEY=val> ...]` |
| [`ASSERT_EQ`](#assert_eq) | `ASSERT_EQ <actual> <expected> \| ASSERT_EQ --hash <sha256> <actual>` |
| [`ASSERT_CONTAINS`](#assert_contains) | `ASSERT_CONTAINS <haystack> <needle>` |
| [`HASH_SHA256`](#hash_sha256) | `HASH_SHA256 <path>` |
| [`EXIT`](#exit) | `EXIT <code>` |
| [`SLEEP`](#sleep) | `SLEEP <duration>` |
| [`LIST_APPEND`](#list_append) | `LIST_APPEND $list <item>` |
| [`WITH_IO`](#with_io) | `WITH_IO [<stream>[=$var], ...] <command> \| WITH_IO [bindings] { <commands> } \| WITH_IO [tty[, stdin=$p][, rows=<int>][, cols=<int>]] RUN ...` |
| [`FOR`](#for) | `FOR $item: TYPE IN <expr> { <commands> } \| FOR $key: STRING, $value: TYPE IN <expr> { <commands> }` |
| [`IF`](#if) | `IF <expr> { <commands> } [ELSE IF <expr> { <commands> } ...] [ELSE { <commands> }]` |
| [`LET`](#let) | `LET $var: TYPE = <expr> \| LET $p: PIPE \| LET $var: TYPE = ASYNC { <commands> } \| LET $var: TYPE = <command> \| LET $var: TYPE = AWAIT $task \| LET $var: TYPE = { <commands> }` |
| [`MUTATION`](#mutation) | `$var = <expr>` |
| [`ASYNC`](#async) | `ASYNC <command...> \| ASYNC { <commands> } \| LET $var: HANDLE = ASYNC { <commands> }` |
| [`AWAIT`](#await) | `AWAIT $var \| LET $out: STRING = AWAIT $var` |
| [`CANCEL`](#cancel) | `CANCEL $var` |
| [`TIMEOUT`](#timeout) | `TIMEOUT <duration> <command...> \| TIMEOUT <duration> { <commands> } \| TIMEOUT <duration> AWAIT $var` |
| [`ISOLATED`](#isolated) | `ISOLATED { INHERIT_ENV [...] <commands> } \| ISOLATED <command...>` |
| [`REMOTE`](#remote) | `REMOTE <target> [[$var, ...] [env:NAME, ...]] { <commands> }` |
| [`FUNC`](#func) | `FUNC NAME([$param: TYPE, ...]) { <commands> }` |
| [`RETURN`](#return) | `RETURN [<expr>]` |
| [`WHILE`](#while) | `WHILE <bool-expr> { <commands> }` |
| [`BREAK`](#break) | `BREAK` |
| [`CONTINUE`](#continue) | `CONTINUE` |
| [`IMPORT`](#import) | `IMPORT [<module>, ...] \| IMPORT <module>` |

### WITH_IO

Reroute standard streams.

**Syntax:** `WITH_IO [<stream>[=$var], ...] <command> | WITH_IO [bindings] { <commands> } | WITH_IO [tty[, stdin=$p][, rows=<int>][, cols=<int>]] RUN ...`

Reroutes the standard streams of the next command or, in block form,
of every enclosed command.

Bindings map streams (`stdin`, `stdout`, `stderr`) to a PIPE-typed
variable (`stdout=$p`, `stdin=$p`), resolved from the variable
when the step runs. Both stdout and stderr pipes capture output
the same way. Declare the handle first with `LET $p: PIPE`.

A `RUN` step can opt into a pseudo-terminal with the bare `tty`
flag instead of a new command: `WITH_IO [tty] RUN [...]`
spawns the child under a pty sized from the host terminal
when it names none (24x80 fallback off-terminal;
`rows = <int>` / `cols = <int>` override each side).
Unnamed sides track the host window while the step runs
(signal-driven on Unix, polled where no window signal
exists); explicit sides stay pinned.
The master carries the child's interleaved stdout and stderr
as one stream, so it routes exclusively to `stdout` (step
capture or a `stdout = $p` pipe): `stderr` bindings alongside
`tty` are rejected, and `tty` outside a `RUN` step is
rejected. `stdin = $p` feeds the master; bare `[tty]`
with a real terminal stdin feeds host keys the same
way (raw, transparent) and stays closed headless. Expect pty translation in assertions:
newlines arrive as carriage return plus newline, and prompts
echo input. An input pipe that EOFs contributes a
terminal-echoed end-of-transmission marker on some
platforms: prefer `ASSERT_CONTAINS` over byte-exact
assertions on fed sessions. Pty-backed descriptors never promote to OS
kernel pipes, even inside `ASYNC` single-`RUN` bodies.

Pipes hold bytes in memory and spill to a temp file above 8 MiB, so a
producer can finish before the consumer starts.

If WITH_IO wraps an ASYNC block whose body is a single RUN, guarded or
not, the pipe is a zero copy OS kernel pipe instead: pair it with a
consumer that runs while the producer is alive, since output past the
64 KiB kernel buffer stalls until drained. That promotion never crosses
a function boundary: pipes created, bound, or passed by variable inside FUNC
bodies are always script pipes, even when the surrounding task would
otherwise promote.

A second producer or consumer on a live handle is an explicit
error. A handle bound as output can later feed another
command's `stdin`, connecting commands without touching the
terminal. Binding `stdout` and `stderr` to the same live
handle fails deterministically. Merge streams in shell
via `2>&1` instead.

Nested blocks stack defaults; inline bindings override inherited ones for
their command only; closing a block restores previous wiring.


**Examples:**

**Example: with_io block**

```oxdock
LET $log: PIPE
WITH_IO [stdout=$log] {
  ECHO first
  ECHO second
}
WITH_IO [stdin=$log] WRITE captured.txt

# The piped bytes landed in the file.
LET $body: STRING = READ captured.txt
ASSERT_CONTAINS $body "first"
ASSERT_CONTAINS $body "second"
```

**Example: variable pipe binding**

```oxdock
# Declare the pipe first: `LET $p: PIPE` mints a fresh
# backend without touching a stream. A plain string here
# would be a TypeMismatch.
LET $p: PIPE
WITH_IO [stdout=$p] ECHO hello
WITH_IO [stdin=$p] READ_LINE $line
ASSERT_EQ $line "hello"
```

**Example: pty terminal binding**

```oxdock
# A RUN step under bare `tty` sees a terminal: newlines
# arrive translated, so assert with CONTAINS, never EQ.
LET $p: PIPE
WITH_IO [tty, stdout=$p] RUN "echo hello-tty"
WITH_IO [stdin=$p] READ_LINE $line
ASSERT_CONTAINS $line "hello-tty"
```


### FOR

Iterate over a list or map.

**Syntax:** `FOR $item: TYPE IN <expr> { <commands> } | FOR $key: STRING, $value: TYPE IN <expr> { <commands> }`

The loop variable receives each element (lists) or value (maps); with
two variables, the first receives the key.

Loop variables are declared with explicit types and scoped per iteration;
they do not leak outward. The body may be a braced block
or a single-line `{ ... }` command.

`GLOB("...")` patterns must be quoted (`*` is not a bare word, so
`GLOB(*)` is a parse error); GLOB returns a root-relative sorted list,
empty when nothing matches, and rejects `..` escapes.


**Examples:**

**Example: for loop**

```oxdock
# Each element binds in turn; the loop body sees every one.
LET $items: LIST<STRING> = ["a", "b"]
FOR $item: STRING IN $items {
  ECHO $item
}
ASSERT_CONTAINS stdout "a"
ASSERT_CONTAINS stdout "b"

# Key and value bind together for maps.
LET $map: MAP<x: INT> = {"x": 1}
FOR $k: STRING, $v: INT IN $map {
  ECHO "{{ $k }}={{ $v }}"
}
ASSERT_CONTAINS stdout "x=1"
```

**Example: expand every match**

```oxdock
# Single-line body; $x is a template path, WHO an override.
IMPORT [STD]
WRITE a.txt "hi \{{ env:WHO }}!"
FOR $x: STRING IN GLOB("*.txt") { EXPAND $x WHO=World }

ASSERT_CONTAINS stdout "hi World!"
```


### IF

Conditional execution.

**Syntax:** `IF <expr> { <commands> } [ELSE IF <expr> { <commands> } ...] [ELSE { <commands> }]`

The condition is evaluated as a boolean expression.

Prefix `!` negates (`IF !false`); `&&` binds tighter than
`||`, and both short-circuit, so `IF true || $missing`
never evaluates the right side. Only Bool values are
accepted as conditions.


**Examples:**

**Example: if else**

```oxdock
IMPORT [STD]

# True branch runs; the false branch is skipped.
IF true {
  WRITE yes.txt taken
} ELSE {
  WRITE yes.txt skipped
}

# ELSE IF selects the first true branch.
IF false {
  WRITE skipped.txt no
} ELSE IF true {
  WRITE fallback.txt taken
}

# !false evaluates to true, so this branch runs.
IF !false {
  WRITE negated.txt taken
}

LET $yes_body: STRING = READ yes.txt
LET $fallback_body: STRING = READ fallback.txt
LET $negated_body: STRING = READ negated.txt
ASSERT_EQ $yes_body "taken"
ASSERT_EQ $fallback_body "taken"
ASSERT_EQ $negated_body "taken"
LET $t: STRING = PATH_TYPE("skipped.txt")
ASSERT_EQ $t "absent"
```

**Example: logical condition composition**

```oxdock
IMPORT [STD]
LET $role: STRING = "admin"
LET $level: INT = 3

# || is true when either side holds; && needs both.
IF $role == "owner" || $level >= 5 {
    WRITE unexpected.txt no
} ELSE {
    WRITE fallback.txt or-false
}

LET $fb: STRING = READ fallback.txt
ASSERT_EQ $fb "or-false"
LET $t1: STRING = PATH_TYPE("unexpected.txt")
ASSERT_EQ $t1 "absent"

IF $role == "admin" || $level >= 5 {
    WRITE chosen.txt or-true
}

LET $ch: STRING = READ chosen.txt
ASSERT_EQ $ch "or-true"

IF $role == "admin" && $level >= 5 {
    WRITE unexpected-too.txt no
} ELSE {
    WRITE and.txt and-false
}

LET $an: STRING = READ and.txt
ASSERT_EQ $an "and-false"
LET $t2: STRING = PATH_TYPE("unexpected-too.txt")
ASSERT_EQ $t2 "absent"
```


### LET

Bind script-local variables.

**Syntax:** `LET $var: TYPE = <expr> | LET $p: PIPE | LET $var: TYPE = ASYNC { <commands> } | LET $var: TYPE = <command> | LET $var: TYPE = AWAIT $task | LET $var: TYPE = { <commands> }`

Declares a script-local variable with an explicit type (STRING, INT,
FLOAT, BOOL, PIPE, LIST, MAP, HANDLE, DURATION, PATH). Duplicate LET
in the same scope frame is a redeclaration error; mutate with
`$var = <expr>`.

Variables are usable in templates (`{{ $var }}`), guards, and
expressions. With `ASYNC`, spawns a background task and stores its
handle (see ASYNC). The `$` sigil on the name is mandatory.

No hoisting: a variable exists only after its LET runs, in
execution order. Reading `$var` before its LET (or after the
block that declared it exits) fails with
`undefined variable $var`. Scopes are a stack of frames and
resolution walks innermost outward, so nothing pre-declares
names. Function bodies read outer variables through the same
walk, but their own LETs never leak out (see FUNC).

The right-hand side is always an expression — literals, lists, maps,
arithmetic (`+ - * /` with `*`/`/` binding tighter, unary `-`,
parentheses), comparisons (`< <= > >=` binding tighter than
`== !=`), logical `&&` (tighter) and `||` with short-circuit,
`!` negation, `env:KEY` reads, `INSPECT($var)` snapshots,
`GLOB("*.md")`, `INT(x)` / `FLOAT(x)` conversions — never a
`{{ ... }}` template; interpolation happens in string values,
not here.
The one exception is pipes: `LET $p: PIPE` with no `=`
and no initializer mints a fresh anonymous backend,
lazily materialized at first binding, so two declarations
never share a channel.

Numbers are numeric literals: `42` binds `INT`, `3.14` binds
`FLOAT`. `Int x Int` stays `INT` (checked, integer division,
so `7 / 2` is `3`); any `Float` operand promotes to `FLOAT`.
Division by zero, overflow, and non-finite results are errors.
Both numeric sides compare numerically (`1 == 1.0` is true);
otherwise `==`/`!=` compare rendered strings and ordering on
non-numerics is a Type Error. Constant subtrees fold at parse
time and dynamic arithmetic compiles to flat RPN with
identical semantics.

Float equality is exact with no epsilon. Floats store decimals
in binary, so a value is exact only when its reduced fraction
has a power-of-2 denominator: 0.5 (1/2), 0.25 (1/4), 0.75
(3/4) are exact, while 0.1 (1/10), 0.2 (1/5), 0.3 (3/10)
repeat forever in binary (like 1/3 in decimal) and truncate,
so `0.1 + 0.2 == 0.3` is false (the sum is
`0.30000000000000004`). Rule of thumb: endings .5, .25, .75,
.125, .625, .875 are exact; .1, .2, .3 and similar are
approximations. Bound approximations instead of comparing
them: `IF $sum > 0.299999 && $sum < 0.300001`.

Comparisons do not chain: `a < b < c` is a parse error, not
`(a < b) < c`. Chaining would compare a `BOOL` against a
number (a runtime Type Error in C-style parsing) or evaluate
the middle term twice (Python-style chaining), so the grammar
accepts exactly one comparison operator per level. Write the
conjunction explicitly: `$a < $b && $b < $c`. The same holds
for equality (`$a == $b == $c` is rejected).

Captured command output is a string, so convert before math:
`LET $total: INT = $total + INT($size_str)` (`INT` trims ASCII
whitespace; `FLOAT` accepts int strings and rejects
non-finite).

Bare words need no quotes: `LET $d: STRING = 30s` binds the same string
as quoted.

When the right-hand side is a synchronous command
(`LET $out: STRING = ECHO hi`), the command runs to completion and its
exact stdout bytes are captured into the variable as a string (no newline
stripping; commands with no stdout capture as `""`; non-UTF8 stdout is
an error). Combining capture with an explicit
`WITH_IO [stdout=$var]` is a parse error.

Coming from Bash, the capture line looks familiar but behaves
strictly:

| | Bash `output=$(...)` | OxDock `LET $out: STRING = ...` |
| --- | --- | --- |
| Trailing newlines | Stripped (all of them) | Preserved byte-exact |
| Variable type | Always an untyped string | Declared: STRING, INT, FLOAT, ... |
| Math on output | Implicit: `$((var + 1))` | Explicit: `INT($out) + 1` |
| Failing command | Continues with empty output unless `set -e` | Step fails immediately, binds nothing |

`LET $out: TYPE = AWAIT $var` binds the background task's
explicit `RETURN` value instead (tasks stream their stdout
live, so there is no output left to capture); a task that
succeeded without `RETURN` yields `INT` 0, like a process
exit status.

An inline block (`LET $var: TYPE = { <commands> }`) runs its
steps in a fresh scope and binds the nearest `RETURN` value,
like a zero-arg function body: fallthrough without `RETURN`
binds `""`, and `BREAK`/`CONTINUE` escaping the block are
errors. Think IIFE: the block executes immediately where it
stands and its value is the result. The block reads outer
variables but its own LETs never leak out. A `{k: v}` shape
still parses as a map literal; anything else in braces is
a block.

The split is deliberate: synchronous commands capture
stdout because they run inline to completion on the same
thread; background tasks never capture stdout because
concurrent output has no well-defined value. Task results
travel only through `RETURN` (or `INT` 0 for void tasks).

`LET $e: STRING = env:FOO` reads the script environment into a plain
string.


**Examples:**

**Example: let**

```oxdock
LET $name: STRING = "world"
ECHO "hello, {{ $name }}"
ASSERT_CONTAINS stdout "hello, world"

LET $items: LIST<STRING> = ["a", "b"]
ASSERT_CONTAINS $items "a"
ASSERT_CONTAINS $items "b"

LET $count: INT = 42
ASSERT_EQ $count 42
```

**Example: no hoisting**

```oxdock expect_error:"undefined variable"
# Reading before the LET runs is an error, not an empty value.
ECHO $too_early
LET $too_early: STRING = "too late"
```

**Expected error:** `undefined variable`

**Example: glob binding**

```oxdock
# The RHS is an expression: GLOB(...) runs and binds a list.
IMPORT [STD]
WRITE a.txt "x"
LET $files: LIST<STRING> = GLOB("*.txt")
FOR $f: STRING IN $files { ECHO $f }

ASSERT_CONTAINS stdout "a.txt"
```

**Example: scoped variable reverts**

```oxdock
# LET inside a braced block reverts when the block exits.
LET $a: STRING = "outer"

[bool:true] {
    LET $a: STRING = "inner"
    WRITE inner.txt "{{ $a }}"
}

WRITE outer.txt "{{ $a }}"

LET $in_body: STRING = READ inner.txt
ASSERT_EQ $in_body "inner"

LET $out_body: STRING = READ outer.txt
ASSERT_EQ $out_body "outer"
```

**Example: capture command output**

```oxdock
# Capture keeps the trailing newline.
LET $out: STRING = ECHO hi
ASSERT_EQ $out "hi\n"
```

**Example: inline block**

```oxdock
LET $who: STRING = "ada"

# An inline block binds its RETURN value like a function body.
LET $res: STRING = {
    LET $loud: STRING = "{{ $who }}!"
    RETURN $loud
}
ASSERT_EQ $res "ada!"

# Any declared type works: the block value checks like any RHS.
LET $n: INT = {
    RETURN 40 + 2
}
ASSERT_EQ $n 42
```

**Example: arithmetic over captured output**

```oxdock
# Captured output converts explicitly: INT() then arithmetic.
IMPORT [STD]
LET $size_str: STRING = ECHO 41
LET $total: INT = INT($size_str) + 1
ASSERT_EQ $total 42

# FLOAT() promotes instead of truncating.
LET $ratio: FLOAT = 1 + 2.5
ASSERT_EQ $ratio 3.5

# Int x Int stays INT: integer division truncates.
LET $half: INT = 7 / 2
ASSERT_EQ $half 3
```

**Example: float equality is exact**

```oxdock
# Binary fractions compare cleanly; decimal fractions may not:
# 0.1 + 0.2 is 0.30000000000000004, so == is false.
IMPORT [STD]
LET $exact: BOOL = 0.5 + 0.25 == 0.75
LET $decimal: BOOL = 0.1 + 0.2 == 0.3
IF $exact {
    WRITE exact.txt yes
}
IF $decimal {
    WRITE unexpected.txt no
}

LET $ok: STRING = READ exact.txt
ASSERT_EQ $ok "yes"

LET $t: STRING = PATH_TYPE("unexpected.txt")
ASSERT_EQ $t "absent"
```

**Example: bound inexact decimals**

```oxdock
# Never test inexact decimals for equality; bound them.
LET $sum: FLOAT = 0.1 + 0.2
IF $sum > 0.299999 && $sum < 0.300001 {
    WRITE bounded.txt yes
}

LET $ok: STRING = READ bounded.txt
ASSERT_EQ $ok "yes"
```

**Example: inspect a variable**

```oxdock
# INSPECT($var) snapshots a variable into a MAP: declared
# type plus live details (pipe backend stats here), so
# scripts can branch on engine state.
IMPORT [STD]
LET $p: PIPE
WITH_IO [stdout=$p] ECHO hello
LET $info: MAP<ANY> = INSPECT($p)
IF $info.is_os_pipe {
    WRITE unexpected.txt "should be a script pipe"
}

ASSERT_EQ $info.type "PIPE"
LET $t: STRING = PATH_TYPE("unexpected.txt")
ASSERT_EQ $t "absent"
```


### MUTATION

Mutate a declared variable.

**Syntax:** `$var = <expr>`

Reassigns an existing variable, converting the new value to
the type declared at LET time. The explicit annotation is
what authorizes string-to-number conversion here (`$n = "42"`
binds 42 for an INT); a non-numeric string is an error.
Expressions never convert: `"100" + 1` is a Type Error, use
`INT()` / `FLOAT()` to cross that boundary explicitly.

The leading `$` distinguishes mutation from `KEY=value` command
assignments. Assigning an undeclared variable or a mismatched type is
an error.

Mutation writes through to the scope where the variable was
declared, so it survives block exit: `LET $x` outside a block
followed by `$x = ...` inside still reads back the new value
afterwards, for every type. This is the counterpart to LET
shadowing, where `LET $x` *inside* the block declares a
separate inner variable that reverts on exit.

Reassignment rebinds the variable to a new value: values
captured earlier keep the old word (see the rebinding
example below).


**Examples:**

**Example: mutate**

```oxdock
# Mutation writes through: the binding holds the new value.
LET $count: INT = 1
$count = 2
ASSERT_EQ $count 2
```

**Example: rebind keeps earlier captures**

```oxdock
# Reassignment rebinds: a map built from `$s` still
# reads the original string after `$s` moves on.
LET $s: STRING = "test"
LET $m: MAP<ANY> = {a: $s}
$s = "changed"
ASSERT_EQ $m.a "test"

# Appending to one list binding never touches its clones.
LET $a: LIST<STRING> = ["1"]
LET $b: LIST<STRING> = $a
LIST_APPEND $a "2"
LET $want: LIST<STRING> = ["1"]
ASSERT_EQ $b $want
```

**Example: convert before math**

```oxdock
# Captured output is a string: `"100" + 1` is a Type Error.
# Convert explicitly, then mutate with arithmetic.
IMPORT [STD]
LET $raw: STRING = ECHO 100
LET $n: INT = INT($raw)
$n = $n + 1

# The declared slot stays strict: strings parse only
# through the explicit converter, never on assignment.
$n = INT("42")
ASSERT_EQ $n 42

# Same crossing for decimals via FLOAT().
LET $frac_str: STRING = ECHO 2.5
LET $f: FLOAT = FLOAT($frac_str) + 0.25
ASSERT_EQ $f 2.75
```


### ASYNC

Run steps in a background thread.

**Syntax:** `ASYNC <command...> | ASYNC { <commands> } | LET $var: HANDLE = ASYNC { <commands> }`

Runs a command or block of commands in a background thread with
subshell isolation.

Mutations (ENV, WORKDIR) stay within the block. With `LET`, stores a
task handle for `AWAIT`. Task output streams live to the parent
stdout; a task publishes a value with an explicit `RETURN`,
which `LET $out: TYPE = AWAIT $task` binds.

`WITH_IO` must wrap `ASYNC` from the outside (`LET $p: PIPE` first,
then `WITH_IO [stdout=$p] ASYNC ...`) so pipe endpoints allocate on
the main thread before the worker spawns. Placing `WITH_IO` directly
inside `ASYNC` is rejected at parse time.


**Examples:**

**Example: async**

```oxdock
# Inline and block forms both run in the background; AWAIT joins them.
ASYNC ECHO "warming-up"
LET $a: HANDLE = ASYNC ECHO "first"
LET $b: HANDLE = ASYNC {
    ECHO "second"
}
AWAIT $a
AWAIT $b
ASSERT_CONTAINS stdout "first"
ASSERT_CONTAINS stdout "second"
```

**Example: async task handle**

```oxdock
LET $task: HANDLE = ASYNC {
    ECHO "built"
}
AWAIT $task
ASSERT_CONTAINS stdout "built"
```


### AWAIT

Join a background task.

**Syntax:** `AWAIT $var | LET $out: STRING = AWAIT $var`

Blocks until the named task completes. Propagates errors if the task failed.

Task output streams live during the run; joining binds nothing by
itself. `LET $out: TYPE = AWAIT $var` binds the task's explicit
`RETURN` value instead, or `INT` 0 when the task succeeded
without one (add `RETURN <expr>` to the task body to yield
a value).


**Examples:**

**Example: await**

```oxdock
LET $task: HANDLE = ASYNC ECHO "done"
AWAIT $task
ASSERT_CONTAINS stdout "done"
```

**Example: await capture**

```oxdock
LET $task: HANDLE = ASYNC {
    ECHO "logged"
    RETURN "returned"
}

# AWAIT binds the RETURN value, not the streamed output.
LET $out: STRING = AWAIT $task
ASSERT_EQ $out "returned"
```


### CANCEL

Synchronously cancel a background task.

**Syntax:** `CANCEL $var`

Kills the named background task spawned via LET $var: HANDLE = ASYNC ....

Blocking: returns only after the task thread has been joined and its OS
process reaped, so no residual filesystem or stream mutation follows. A
later AWAIT $var reports cancellation. Only named tasks can be cancelled.


**Examples:**

**Example: cancel**

```oxdock
LET $task: HANDLE = ASYNC SLEEP 30s
CANCEL $task
```

**Example: await after cancel reports cancellation**

```oxdock expect_error:"was cancelled"
# A cancelled task stays cancelled: joining it reports.
LET $task: HANDLE = ASYNC SLEEP 30s
CANCEL $task
AWAIT $task
```

**Expected error:** `was cancelled`


### TIMEOUT

Enforce an execution deadline.

**Syntax:** `TIMEOUT <duration> <command...> | TIMEOUT <duration> { <commands> } | TIMEOUT <duration> AWAIT $var`

Aborts the wrapped step or block with a deadline error if it exceeds the
duration (e.g. 500ms, 10s, 2m; a bare number means seconds).

A blocking foreground process is killed.

ASYNC composes in either nesting order with order-dependent deadline
semantics: `LET $task: HANDLE = ASYNC TIMEOUT 30s RUN "build"` enforces
the deadline inside the background thread (a later `AWAIT $task`
surfaces the `TIMEOUT` error), while `TIMEOUT 30s AWAIT $task`
preempts a hung task from the awaiting side.


**Examples:**

**Example: timeout**

```oxdock
TIMEOUT 30s WRITE heartbeat.txt alive
LET $beat: STRING = READ heartbeat.txt
ASSERT_EQ $beat "alive"
```

**Example: timeout block**

```oxdock
TIMEOUT 30s {
    WRITE a.txt one
    WRITE b.txt two
}
LET $a: STRING = READ a.txt
LET $b: STRING = READ b.txt
ASSERT_EQ $a "one"
ASSERT_EQ $b "two"
```

**Example: deadline aborts the step**

```oxdock expect_error:"TIMEOUT after"
# 50ms expires long before the sleep does: the step dies
# with a deadline error instead of running out the clock.
TIMEOUT 50ms SLEEP 30s
```

**Expected error:** `TIMEOUT after`

**Example: timeout variable duration**

```oxdock
# Durations resolve at runtime, so variables work too.
LET $budget: DURATION = "30s"
TIMEOUT $budget WRITE heartbeat.txt alive

LET $beat: STRING = READ heartbeat.txt
ASSERT_EQ $beat "alive"
```

**Example: timeout await**

```oxdock
# AWAIT form bounds a task join instead of a step.
LET $quick: HANDLE = ASYNC {
    ECHO hi
}
TIMEOUT 30s AWAIT $quick
```


### ISOLATED

Run steps with the seeded script environment.

**Syntax:** `ISOLATED { INHERIT_ENV [...] <commands> } | ISOLATED <command...>`

Resets the script environment for the block to the session
seed: spawned children see the sandbox defaults plus names
the block re-adds through its leading `INHERIT_ENV`.

The allowlist resolves against the outer scope, never the
host. Absent names stay absent silently, matching top-level
`INHERIT_ENV`.

Script variables, pipes, and files flow through untouched;
only `env:` is reset. Files share the session workspace:
blocks selecting the same `WORKSPACE` target resolve to the
same directory with no per-block overlay, so one block's
writes are visible to another.

Blocks nest (each level re-seeds from the same seed, so
nesting only narrows through the allowlists) and compose
with `ASYNC` and `FUNC`. Scope exit restores the prior
environment.


**Examples:**

**Example: isolated scope**

```oxdock
# The block sees a clean env; plain steps still run and
# outer bindings survive untouched.
ENV MODE=outer
ISOLATED {
    ENV MODE=inner
    WRITE inner.txt "{{ env:MODE }}"
}
LET $back: STRING = "{{ env:MODE }}"
ASSERT_EQ $back "outer"
LET $body: STRING = READ inner.txt
ASSERT_EQ $body "inner"
```

**Example: reseeded child env**

```oxdock
# The block resets to the session seed: plain steps and
# spawns run with zero prelude, while outer customs
# stay out.
ENV OUTER_CUSTOM=outer-val
WRITE top-path.txt "{{ env:PATH }}"
ISOLATED {
    WRITE inner-path.txt "{{ env:PATH }}"
    WRITE inner-custom.txt "{{ env:OUTER_CUSTOM }}"
    RUN "echo seed-ok"
}
LET $top: STRING = READ top-path.txt
LET $inner: STRING = READ inner-path.txt
ASSERT_EQ $inner $top
LET $custom: STRING = READ inner-custom.txt
ASSERT_EQ $custom ""
ASSERT_CONTAINS stdout "seed-ok"
```

**Example: absent names stay absent**

```oxdock
# Listing a name the outer scope lacks is not an error:
# the block simply runs without it.
ISOLATED {
    INHERIT_ENV [OXDOC_DEFINITELY_ABSENT_12345]
    ECHO still-runs
}
ASSERT_CONTAINS stdout "still-runs"
```

**Example: nesting narrows**

```oxdock
# Each level snapshots the already-scrubbed parent, so
# an inner block cannot recover what the outer dropped.
ENV KEEP=yes
ISOLATED {
    INHERIT_ENV [KEEP]
    ISOLATED {
        ENV KEEP=inner
        WRITE deep.txt "{{ env:KEEP }}"
    }
    LET $mid: STRING = "{{ env:KEEP }}"
    ASSERT_EQ $mid "yes"
}
LET $deep: STRING = READ deep.txt
ASSERT_EQ $deep "inner"
```


### REMOTE

Run a sealed block on a remote target.

**Syntax:** `REMOTE <target> [[$var, ...] [env:NAME, ...]] { <commands> }`

Runs the braced block on the named remote target, which the
host binds at startup (`--remote`). The block is sealed:
only header-listed `$var` and `env:NAME` entries cross, the
guest starts empty, and results return through declared
transfers or `WITH_IO` pipes.

The target is a static literal, never a variable. Unknown
targets fail before execution starts.

Bodies check twice: the host static pass validates the
body before anything ships, and the guest re-validates on
arrival through the same entry point, so a body type error
fails locally, never mid-run on the remote.

A failing guest fails the host step: EXIT in the body
exits the guest, and the host relays the code to its own
process exit status (see EXIT).


**Examples:**

**Example: sealed header values cross, outer scope does not**

```oxdock mock_remote:prod
# Only header-listed names arrive: `$version` is visible
# on the guest, `$secret` was never sent.
LET $version: STRING = "1.2.3"
LET $secret: STRING = "host-only"
REMOTE prod [$version] {
    WRITE version.txt "{{ $version }}"
    COPY --to-host ./version.txt back.txt
}

# The pushed file proves the guest saw the injected value.
LET $back: STRING = READ back.txt
ASSERT_EQ $back "1.2.3"
```

**Example: background tasks stream through shared pipes**

```oxdock mock_remote:prod
# Two background REMOTEs share one capture pipe while a
# WITH_IO block holds the write end open.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    LET $a: HANDLE = ASYNC REMOTE prod {
        ECHO "from-a"
    }
    LET $b: HANDLE = ASYNC REMOTE prod {
        ECHO "from-b"
    }
    AWAIT $a
    AWAIT $b
}

# EOF ends the drain: no sentinel line, no baked-in count.
LET $n: INT = 0
WHILE !EOF($cap) {
    WITH_IO [stdin=$cap] READ_LINE $line
    $n = $n + 1
}
ASSERT_EQ $n 2
```

**Example: declared files cross both directions**

```oxdock mock_remote:prod roots:unified
# Fetches ride in, pushes ride back. Anything undeclared
# never leaves its side.
WRITE send.txt "payload"
REMOTE prod {
    COPY --from-host ./send.txt got.txt
    LET $v: STRING = READ got.txt
    ASSERT_EQ $v "payload"
    WRITE back.txt "returned"
    COPY --to-host ./back.txt back.txt
}

# The push landed on the host; guest staging is gone.
LET $b: STRING = READ back.txt
ASSERT_EQ $b "returned"
```

**Example: guest workspace selection stays on the guest**

```oxdock mock_remote:prod
# WORKSPACE switches inside the block resolve against the
# guest staging dir, which serves as both the SNAPSHOT
# root and the LOCAL build context. The host selection
# restores when the block exits.
REMOTE prod {
    WORKSPACE SNAPSHOT
    WRITE out.txt "from-snapshot"
    COPY --to-host ./out.txt snap.txt
}

LET $s: STRING = READ snap.txt
ASSERT_EQ $s "from-snapshot"
```

**Example: guest cache persists across execs**

```oxdock mock_remote:prod
# WORKSPACE CACHE addresses guest-local persistent
# storage: the second block reads what the first wrote,
# while the guest staging starts empty every time.
REMOTE prod {
    WORKSPACE CACHE
    WRITE marker.txt "run-marker"
}
REMOTE prod {
    WORKSPACE CACHE
    LET $m: STRING = READ marker.txt
    ASSERT_EQ $m "run-marker"
}
```

**Example: pushes outside staging fail closed**

```oxdock mock_remote:prod expect_error:"escapes staging"
# WORKSPACE SYSTEM exposes the whole guest machine, but
# transfers stay staging scoped: a push source outside
# staging fails loudly instead of crossing.
REMOTE prod {
    WORKSPACE SYSTEM
    COPY --to-host /no/such/file.txt got.txt
}
```

**Expected error:** `escapes staging`

**Example: guest exit fails the step, host survives**

```oxdock mock_remote:prod expect_error:"REMOTE 'prod' exited with code 3"
# EXIT in the body exits the guest with code 3. The
# host is unaffected: it keeps executing after the
# guest is already dead, and the failure surfaces as
# a host step error carrying the same code. The CLI
# then exits with it, so the guest code reaches the OS.
LET $t: HANDLE = ASYNC REMOTE prod {
    EXIT 3
}
WRITE host-alive.txt "still here"
AWAIT $t
```

**Expected error:** `REMOTE 'prod' exited with code 3`

**Example: remote block without a runner**

```oxdock expect_error:"has no registered session"
# Without a bound session, the block bails instead of
# executing anywhere unexpected.
REMOTE prod {
    ECHO "do something on remote"
}
```

**Expected error:** `has no registered session`


### FUNC

Define a user function.

**Syntax:** `FUNC NAME([$param: TYPE, ...]) { <commands> }`

Defines a user function with UPPERCASE name and explicitly typed
parameters.

Params bind by position, converting each argument to its
declared parameter type before the body runs.
Bodies run in a fresh variable scope; LETs inside do not leak. A nested
FUNC definition is scoped to its block and reverts on exit. Names share
one namespace with native and host-registered functions, which a FUNC
may never shadow.

Functions resolve like variables: a name is visible from its
definition line, so recursion works but mutual recursion does
not (the second name does not exist while the first body
lowers). Calls name their module (`STD::GLOB(...)`) unless
imported; see IMPORT.

Invoke any function with one syntax: `NAME(...)` as a statement
(discarding the value) or `LET $var: TYPE = NAME(...)` to capture
the RETURN value (fallthrough without RETURN captures as "").


**Examples:**

**Example: func def call**

```oxdock
FUNC GREET($name: STRING) {
  RETURN $name
}

LET $res: STRING = GREET("ada")
ASSERT_EQ $res "ada"

# Statement form: parens stay, the value drops.
GREET("bex")
```

**Example: call with pipes**

```oxdock
# A pipe handle travels into a function as a typed argument
# and is usable as a binding target in both directions.
# `LET $p: PIPE` mints the handle; `$p` passes it on.
FUNC DRAIN($q: PIPE) {
  WITH_IO [stdin=$q] READ_LINE $line
  RETURN $line
}

LET $p: PIPE
WITH_IO [stdout=$p] ECHO "payload"

LET $got: STRING = DRAIN($p)
ASSERT_EQ $got "payload"
```


### RETURN

Return a value from a function, task, or inline block.

**Syntax:** `RETURN [<expr>]`

Ends the nearest enclosing boundary with a value: a function
call, an `ASYNC` task (bound by `LET $o = AWAIT $t`), or an
inline `LET` block. Bare `RETURN` with no expression yields
`""`.

Falling off the end without RETURN yields "". RETURN with no
enclosing boundary (including at top level) is an error; use
EXIT or ECHO there.


**Examples:**

**Example: return**

```oxdock
FUNC PICK($flag: BOOL) {
  IF $flag {
    RETURN "yes"
  }
  RETURN "no"
}

LET $res: STRING = PICK(true)
ASSERT_EQ $res "yes"

# Fallthrough without RETURN yields its own value.
LET $no: STRING = PICK(false)
ASSERT_EQ $no "no"
```


### WHILE

Loop while a condition holds.

**Syntax:** `WHILE <bool-expr> { <commands> }`

Re-evaluates a Bool condition each iteration (same is_truthy rule as IF;
non-Bool is a type error).

Each iteration runs in a fresh scope; mutate outer state with $var = ...
so the next check observes it. BREAK exits the loop; CONTINUE skips to
the next check.


**Examples:**

**Example: while loop**

```oxdock
# The condition re-evaluates every iteration: three passes, then stop.
LET $n: INT = 0
WHILE $n < 3 {
  WRITE tick.txt "{{ $n }}"
  $n = $n + 1
}

ASSERT_EQ $n 3
LET $tick: STRING = READ tick.txt
ASSERT_EQ $tick "2"
```


### BREAK

Exit the innermost loop.

**Syntax:** `BREAK`

Exits the innermost enclosing FOR or WHILE loop.

BREAK outside a loop, or across a FUNC or ASYNC boundary, is an error.


**Examples:**

**Example: break**

```oxdock
# BREAK leaves after the first pass: only "a" is written.
FOR $x: STRING IN ["a", "b"] {
  WRITE picked.txt "{{ $x }}"
  BREAK
}

LET $body: STRING = READ picked.txt
ASSERT_EQ $body "a"
```


### CONTINUE

Skip to the next loop iteration.

**Syntax:** `CONTINUE`

Skips the rest of the innermost enclosing FOR or WHILE body and starts
the next iteration.

CONTINUE outside a loop, or across a FUNC or ASYNC boundary, is an error.


**Examples:**

**Example: continue**

```oxdock
# CONTINUE skips the write on "a": only "b" lands.
FOR $x: STRING IN ["a", "b"] {
  IF $x == "a" {
    CONTINUE
  }
  WRITE picked.txt "{{ $x }}"
}

LET $body: STRING = READ picked.txt
ASSERT_EQ $body "b"
```


### IMPORT

Bring module functions into bare-call scope.

**Syntax:** `IMPORT [<module>, ...] | IMPORT <module>`

Every function call names its module (`STD::GLOB(...)`,
`MOCK::READ_CSV(...)`) unless the module is imported:
`IMPORT [STD]` lets the rest of the scope call `GLOB(...)`
bare. Calls resolve at parse time against `SCRIPT`
definitions first, then imported modules; unknown modules,
unknown functions, and unimported bare calls are parse
errors, never runtime surprises.

IMPORT is a lowering directive, not a step: it applies from
its line to the enclosing block exit, then reverts, exactly
like `LET` scoping but with no runtime footprint. Guards do
not apply to it. Two imported modules exporting one name is
an ambiguity error: qualify the call instead.

`EXPORT` marks top-level script `FUNC`s as externally
callable tools (`EXPORT FOO`, `EXPORT [FOO, BAR]`).
Single-file scope; see the MCP host for consumption.


**Examples:**

**Example: import**

```oxdock
# Calls name their module (STD::GLOB); IMPORT [STD] drops the prefix.
WRITE a.txt "hi \{{ env:WHO }}!"
IMPORT [STD]
FOR $x: STRING IN GLOB("*.txt") { EXPAND $x WHO=World }
ASSERT_CONTAINS stdout "hi World!"
```


### WORKDIR

Change the working directory.

**Syntax:** `WORKDIR <path>`

Sets the current working directory.

Relative paths resolve against the current directory; `/` resets to
the workspace root. Paths cannot escape the workspace.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Directory to change to |

**Examples:**

**Example: change working directory**

```oxdock
# Later relative paths resolve under the new directory.
WORKDIR project/src
WRITE generated.txt generated-under-workdir

LET $body: STRING = READ generated.txt
ASSERT_EQ $body "generated-under-workdir"
```

**Example: workdir in a scoped block**

```oxdock
# The block reverts to the starting directory on exit.
LET $outside: STRING = CWD
MKDIR project

[bool:true] {
    WORKDIR project
    WRITE inner.txt inner
}

LET $back: STRING = CWD
ASSERT_EQ $back $outside
LET $body: STRING = READ project/inner.txt
ASSERT_EQ $body "inner"
```


### WORKSPACE

Switch workspace roots.

**Syntax:** `WORKSPACE (SNAPSHOT|LOCAL|CACHE|SYSTEM) [--local]`

Switches the workspace root. The selection reverts at scope
exit like `WORKDIR`.

- `SNAPSHOT`: the materialized build snapshot (the default).
  One session-shared directory: every selection resolves to
  it, in any block.
- `LOCAL`: the local workspace directory.
- `CACHE`: a persistent per-project directory shared across
  runs, never evicted. It lives under the OS user cache
  (`OXDOCK_CACHE_DIR` pins an exact directory);
  `WORKSPACE CACHE --local` keeps it in
  `<project>/.cache/workspace` instead.
- `SYSTEM`: full filesystem access. Scripts using it are not
  hermetic.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `target` | `SNAPSHOT\|LOCAL\|CACHE\|SYSTEM` | yes | Target root |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--local` | `BOOL` | Use the project-local cache directory instead of the OS user cache (CACHE only) |

**Examples:**

**Example: switch roots**

```oxdock
IMPORT [STD]
WORKSPACE LOCAL

LET $t: STRING = PATH_TYPE(".")
ASSERT_EQ $t "dir"
```

**Example: workspace cache in a scoped block**

```oxdock
[bool:true] {
    WORKSPACE CACHE
    WRITE cached.txt cached-content
}

COPY --from-workspace CACHE cached.txt restored.txt
LET $body: STRING = READ restored.txt
ASSERT_EQ $body "cached-content"
```


### ENV

Set an environment variable.

**Syntax:** `ENV KEY=value`

Inserts or updates an env var.

The value uses the unified string-value rules shared by every command:
`"..."` or `'...'` quotes keep exact bytes (spaces, tabs), a lone `$var`
evaluates that variable, `{{ ... }}` placeholders interpolate, unquoted
words join with single spaces, and the first `=` splits key from value
(`KEY=a=b` stores `a=b`).

A `$var` inside larger text stays literal — write `{{ $var }}` to
interpolate there.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `assignment` | [`STRING`](#value-type-string) | yes | KEY=value pair; the value resolves as STRING |

**Examples:**

**Example: set env**

```oxdock
ENV APP_MODE=production
LET $mode: STRING = env:APP_MODE
ASSERT_EQ $mode "production"
```

**Example: quoted value with spaces**

```oxdock
# Quotes keep the space: SET_FORTH stores `outer scope`.
ENV SET_FORTH="outer scope"
WRITE out.txt "{{ env:SET_FORTH }}"

LET $body: STRING = READ out.txt
ASSERT_EQ $body "outer scope"
```

**Example: variable value**

```oxdock
# A lone $var evaluates, like ECHO $var.
LET $who: STRING = "Alice"
ENV GREETING=$who
WRITE out.txt "{{ env:GREETING }}"

LET $body: STRING = READ out.txt
ASSERT_EQ $body "Alice"
```

**Example: all value forms agree**

```oxdock
# A bare variable, a quoted literal, and a template all
# store plain strings through the same value rules.
LET $x: STRING = "Ada"
ENV A=$x
ENV B="hello world"
ENV C="{{ $x }} concatenated"
WRITE check.txt "{{ env:A }}|{{ env:B }}|{{ env:C }}"

LET $body: STRING = READ check.txt
ASSERT_EQ $body "Ada|hello world|Ada concatenated"
```

**Example: scoped env reverts**

```oxdock
# ENV inside a braced block reverts when the block exits
ENV MODE=production

[bool:true] {
    ENV MODE=staging
    WRITE inner.txt "{{ env:MODE }}"
}

WRITE outer.txt "{{ env:MODE }}"

LET $inner_body: STRING = READ inner.txt
ASSERT_EQ $inner_body "staging"

LET $outer_body: STRING = READ outer.txt
ASSERT_EQ $outer_body "production"
```

**Example: shell reads env per platform**

```oxdock
# A shell command reads its own environment, with
# per-platform spelling: quoted "$VAR" passes the parser
# through untouched on Unix ...
ENV PROXY_PORT=23791

[family:unix] LET $o: STRING = RUN echo serving on "$PROXY_PORT"

# ... while cmd expands %VAR% on Windows.
[family:windows] LET $o: STRING = RUN echo serving on %PROXY_PORT%

ASSERT_CONTAINS $o "23791"
```


### INHERIT_ENV

Inherit env vars from host.

**Syntax:** `INHERIT_ENV [<key>, ...]`

Declares which host environment variables to inherit into the script.

Must appear before any other commands. Several directives may
appear so platform-specific names ride explicit guards
(`[family:windows] INHERIT_ENV [USERPROFILE]`); each guarded
line runs only when its guard passes.

Sandbox defaults need no declaration: loader bindings,
terminal defaults, the host lookup path, and the user's
profile locations seed automatically. See Environment
Variable Contracts for the exact list, which grows with the
engine.

Secrets and tool config stay opt-in here: a token or a
`CARGO_HOME` override arrives only through this directive.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `keys` | [`STRING...`](#value-type-string) | no | Host variables to inherit |

**Examples:**

**Example: inherit env**

```oxdock
[family:unix] INHERIT_ENV [HOME]
LET $path: STRING = env:PATH
[family:unix] ASSERT_CONTAINS $path ":"
[family:windows] ASSERT_CONTAINS $path ";"
```


### ECHO

Print to stdout.

**Syntax:** `ECHO <message>`

Outputs message to stdout.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `message` | [`STRING...`](#value-type-string) | yes | Text |

**Output:** Stdout

**Examples:**

**Example: echo**

```oxdock
ECHO build-complete
ASSERT_CONTAINS stdout "build-complete"
```

**Example: variables**

```oxdock
# {{ }} interpolates inside text; a lone $var evaluates on its own.
LET $x: STRING = "World"
ECHO "braced:{{ $x }}"
ECHO $x
ASSERT_EQ stdout "braced:World\nWorld\n"
```

**Example: interpolation**

```oxdock
# `{{ env:KEY }}` reads the script environment; a bare name
# resolves a DSL variable instead. Unknown keys expand to
# empty with no host fallback.
ENV USER=OxDock
LET $WHO: STRING = "Ada"
ECHO "Hello {{ env:USER }}! Hi {{ WHO }}!{{ env:OXDOCK_DOC_NO_SUCH_KEY }}"
ASSERT_CONTAINS stdout "Hello OxDock! Hi Ada!"
```


### RUN

Execute shell command or direct executable.

**Syntax:** `RUN <command...> | RUN ["exe", "arg", ...]`

Shell form (`RUN <command...>`) runs the joined command string in the
batch shell (`sh -c` / `cmd /C`). Never the operator's login
shell: login shells re-pollute scrubbed children from system
profiles and break script portability, so step execution is
pinned to POSIX `sh` (interactive `oxdock --shell` still uses
`$SHELL`).

Exec form (`RUN ["exe", "arg", ...]`) spawns the executable directly
with no shell, so there is no shell expansion, globbing, redirection,
or pipes; use it for portable commands.

Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`, `LET`) apply to
both forms.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | [`STRING...`](#value-type-string) | yes | Command |

**Examples:**

**Example: run**

```oxdock
# Spawned children resolve through the seeded host PATH:
# closed child env is the default, not ambient inherit.
RUN echo hello

# Captured runs prove the output, not just the exit status.
LET $o: STRING = RUN echo hello
ASSERT_CONTAINS $o "hello"
```

**Example: run exec form**

```oxdock
# No shell: `>` stays a literal argument, so no file is created.
# Toolchain spawns bridge the host home explicitly: rustup
# shims resolve toolchains through the host home.
IMPORT [STD]
[family:unix] INHERIT_ENV [HOME]
[family:windows] INHERIT_ENV [USERPROFILE]
RUN ["cargo", "--version", ">", "x.txt"]
ASSERT_CONTAINS stdout "cargo"

LET $t: STRING = PATH_TYPE("x.txt")
ASSERT_EQ $t "absent"
```


### COPY

Copy file into workspace.

**Syntax:** `COPY [--from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM] [--from-host | --to-host] <from> <to>`

Copies from host (the source is never moved or modified). Docker destination semantics: a file copied onto a directory (an existing one, or a trailing-slash spell like `out/`) is duplicated inside it under its own basename; a directory source duplicates its contents into the destination; any other destination path is created holding the copied bytes.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`PATH`](#value-type-path) | yes | Source |
| `to` | [`PATH`](#value-type-path) | yes | Dest |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-workspace` | `STRING` | Copy from the given workspace root instead of the build context |
| `--from-host` | `BOOL` | Fetch from the host workspace into the guest (REMOTE bodies only) |
| `--to-host` | `BOOL` | Push from the guest workspace back to the host (REMOTE bodies only) |

**Examples:**

**Example: copy**

```oxdock roots:unified
# Copy to a new name, then read back.
WRITE src.txt content
COPY src.txt dst.txt

LET $body: STRING = READ dst.txt
ASSERT_EQ $body "content"
```

**Example: copy from workspace**

```oxdock
# Same name, different contents per root: only LOCAL has ws-content.
WRITE shared.txt from-snapshot
WORKSPACE LOCAL
WRITE shared.txt ws-content

WORKSPACE SNAPSHOT
COPY --from-workspace LOCAL shared.txt ws-copy.txt

LET $body: STRING = READ ws-copy.txt
ASSERT_EQ $body "ws-content"
```


### COPY_GIT

Copy from git revision.

**Syntax:** `COPY_GIT [--include-dirty] <rev> <src> <dst>`

Checkout and copy.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `rev` | [`STRING`](#value-type-string) | yes | Rev |
| `src` | [`PATH`](#value-type-path) | yes | Src |
| `dst` | [`PATH`](#value-type-path) | yes | Dst |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--include-dirty` | `BOOL` | Include dirty |

**Examples:**

**Example: git copy missing source errors**

```oxdock expect_error:"COPY source missing"
COPY_GIT HEAD src.txt dst.txt
```

**Expected error:** `COPY source missing`


### SYMLINK

Create symlink.

**Syntax:** `SYMLINK [--from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM] <from> <to>`

Creates symlink. A directory destination (existing, or a trailing-slash spell) receives the link under the source basename.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`PATH`](#value-type-path) | yes | Target |
| `to` | [`PATH`](#value-type-path) | yes | Link |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--from-workspace` | `STRING` | Symlink from the given workspace root instead of the build context |

**Examples:**

**Example: symlink**

```oxdock roots:unified
# A symlink reads like its target.
WRITE original.txt content
SYMLINK original.txt link.txt

LET $body: STRING = READ link.txt
ASSERT_EQ $body "content"
```

**Example: symlink from workspace**

```oxdock
# Same name, different contents per root: only LOCAL has ws-content.
WRITE shared.txt from-snapshot
WORKSPACE LOCAL
WRITE shared.txt ws-content

WORKSPACE SNAPSHOT
SYMLINK --from-workspace LOCAL shared.txt ws-link.txt

LET $body: STRING = READ ws-link.txt
ASSERT_EQ $body "ws-content"
```


### MKDIR

Create directory.

**Syntax:** `MKDIR <path>`

Creates dir with parents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Dir path |

**Examples:**

**Example: mkdir**

```oxdock
IMPORT [STD]
MKDIR deeply/nested/tree

LET $t: STRING = PATH_TYPE("deeply/nested/tree")
ASSERT_EQ $t "dir"
```


### INSTALL

Atomically install a verified binary as executable.

**Syntax:** `INSTALL <source> <dest>`

Stages the source as a temp file in the dest dir, sets executable mode, then renames into place.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | [`PATH`](#value-type-path) | yes | Source file |
| `to` | [`PATH`](#value-type-path) | yes | Dest file |

**Examples:**

**Example: install**

```oxdock roots:unified
# Place a verified binary with executable mode.
IMPORT [STD]
WRITE src.bin payload
INSTALL src.bin dst-bin/tool

LET $t: STRING = PATH_TYPE("dst-bin/tool")
ASSERT_EQ $t "file"
```


### ARCHIVE

Pack a file or directory into a tarball.

**Syntax:** `ARCHIVE [--raw] <src> <dst>`

Packs a file or directory into a tarball, gzip-compressed by
default (`--raw` writes an uncompressed tar instead).

A lone file packs as a single top-level entry under its
basename; a directory recurses, preserving empty dirs. Source
modes ride in the headers on Unix, so system `tar` restores
them elsewhere. Symlink sources refuse: archives cross trust
boundaries, so links never resolve into member bytes. Hashing
stays with `HASH_SHA256`, which reads the finished archive
like any other file.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `src` | [`PATH`](#value-type-path) | yes | File or directory to pack |
| `dst` | [`PATH`](#value-type-path) | yes | Archive file to write |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--raw` | `BOOL` | Write an uncompressed tar instead of gzip |

**Examples:**

**Example: archive directory**

```oxdock
# Stage a small tree to pack.
WRITE bundle/app.txt bundled-payload
WRITE bundle/nested/deep.txt deep-payload

# ARCHIVE packs it gzip-compressed by default.
ARCHIVE bundle bundle.tar.gz

# UNARCHIVE restores the tree under the destination.
UNARCHIVE bundle.tar.gz restored

# The payloads arrive byte-identical.
LET $body: STRING = READ restored/app.txt
ASSERT_EQ $body "bundled-payload"

LET $deep: STRING = READ restored/nested/deep.txt
ASSERT_EQ $deep "deep-payload"
```

**Example: archive raw**

```oxdock
# A lone file packs as a single top-level entry.
WRITE tool.bin tool-payload

# Raw archives skip gzip and stay readable by any tar.
ARCHIVE --raw tool.bin tool.tar
UNARCHIVE tool.tar tool-out

# The entry lands under its basename.
LET $body: STRING = READ tool-out/tool.bin
ASSERT_EQ $body "tool-payload"
```


### UNARCHIVE

Unpack a tarball into a directory.

**Syntax:** `UNARCHIVE <src> <dst> [--max-bytes <bytes>]`

Unpacks a tarball under the destination directory, creating
it first. Gzip and raw (`ARCHIVE --raw`) tarballs
self-identify by magic bytes, so no flag is needed.

Member bytes meter against a 1 GiB default expansion cap
across the whole archive: a decompression bomb trips it
instead of filling the disk, and `--max-bytes` moves the
ceiling either way (plain integer bytes). The tripped member
stays truncated on disk while the step fails loudly.

Symlink and special members refuse naming the entry, and
absolute or escaping paths fail closed before any byte
lands: the member sanitizer plus the destination join check
form two independent walls. Unix modes recorded in the
headers restore on unpack; other platforms land default
modes (confer executability with `INSTALL` there).


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `src` | [`PATH`](#value-type-path) | yes | Archive file to unpack |
| `dst` | [`PATH`](#value-type-path) | yes | Directory to unpack under |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--max-bytes` | `STRING` | Cap total unpacked member bytes: bare digits are bytes (`1_000_000` grouping allowed), suffixes B/KB/MB/GB/TB (decimal) and KIB/MIB/GIB/TIB (binary), any case, one optional space before the unit (defaults to 1 GiB) |

**Examples:**

**Example: unarchive**

```oxdock
# Pack a note, then restore it under a fresh destination.
WRITE pack/note.txt packed-note
ARCHIVE pack pack.tar.gz
UNARCHIVE pack.tar.gz unpacked

LET $body: STRING = READ unpacked/note.txt
ASSERT_EQ $body "packed-note"
```

**Example: unarchive with a lower cap**

```oxdock
# Small payloads clear even a tight ceiling.
WRITE pack/note.txt packed-note
ARCHIVE pack pack.tar.gz
UNARCHIVE pack.tar.gz capped --max-bytes 100KB

LET $body: STRING = READ capped/note.txt
ASSERT_EQ $body "packed-note"
```


### LS

List directory.

**Syntax:** `LS [<path>]`

Lists entries.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | Dir |

**Output:** Stdout

**Examples:**

**Example: ls**

```oxdock
MKDIR inventory
WRITE inventory/a.txt a
LS inventory
ASSERT_CONTAINS stdout "a.txt"
```


### CWD

Print working directory.

**Syntax:** `CWD`

Outputs cwd.

**Output:** Stdout

**Examples:**

**Example: cwd**

```oxdock
CWD

# CWD tracks WORKDIR: the listing names the new directory.
MKDIR sub
WORKDIR sub
LET $c: STRING = CWD
ASSERT_CONTAINS $c "sub"
```


### READ

Read file to stdout.

**Syntax:** `READ [<path>]`

Outputs file contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | File |

**Output:** Stdout

**Examples:**

**Example: read**

```oxdock
WRITE note.txt "hello"
READ note.txt

LET $body: STRING = READ note.txt
ASSERT_EQ $body "hello"
```


### READ_LINE

Read one line from stdin into a variable.

**Syntax:** `READ_LINE $var`

Reads bytes until newline without waiting for EOF, leaving the pipe open.

Trailing newline is stripped (shell-read parity). On premature EOF
assigns accumulated bytes and returns.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `var` | [`STRING`](#value-type-string) | yes | Target variable (`$name`); the line binds as STRING |

**Examples:**

**Example: read line**

```oxdock
# The trailing newline is stripped: the variable holds exactly `first`.
LET $lines: PIPE
WITH_IO [stdout=$lines] ECHO "first"
WITH_IO [stdin=$lines] READ_LINE $reply
ASSERT_EQ $reply "first"
```


### PROMPT

Prompt on the terminal into a variable.

**Syntax:** `PROMPT $var <message> [<default>]`

Prints `{message} [{default}]: ` (or `{message}: ` with no
default) to stderr and reads one line from the terminal.

Empty input yields the default when one is given, else an
empty string. EOF aborts loudly. Non-TTY stdin never reads:
a default binds with a stderr note, while no default bails.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `var` | [`STRING`](#value-type-string) | yes | Target variable (`$name`); the answer binds as STRING |
| `message` | [`STRING`](#value-type-string) | yes | Prompt message |
| `default` | [`STRING`](#value-type-string) | no | Default on empty input |

**Examples:**

**Example: prompt needs a terminal**

```oxdock expect_error:"non-TTY"
# A redirected stdin can never answer: without a default the
# prompt fails instead of hanging or reading piped stdin.
LET $p: PIPE
WITH_IO [stdin=$p] PROMPT $dir "Install destination?"
```

**Expected error:** `non-TTY`


### WRITE

Write to file.

**Syntax:** `WRITE <path> [<contents>]`

Writes contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |
| `contents` | [`STRING...`](#value-type-string) | no | Content |

**Examples:**

**Example: write**

```oxdock
WRITE output.txt hello-world
LET $body: STRING = READ output.txt
ASSERT_EQ $body "hello-world"
```


### APPEND

Append to file.

**Syntax:** `APPEND <path> [<contents>]`

Appends contents.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |
| `contents` | [`STRING...`](#value-type-string) | no | Content |

**Examples:**

**Example: append**

```oxdock
WRITE log.txt line1
APPEND log.txt line2

# APPEND concatenates with no separator.
LET $all: STRING = READ log.txt
ASSERT_EQ $all "line1line2"
```


### REMOVE

Remove a file, symlink, or directory.

**Syntax:** `REMOVE [--recursive] <path>`

Removes a file, symlink (the link itself), or empty dir. With --recursive removes a directory tree.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | Path to remove |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--recursive` | `BOOL` | Remove a directory tree |

**Examples:**

**Example: remove**

```oxdock
# WRITE then REMOVE leaves nothing behind.
IMPORT [STD]
WRITE scratch.txt content

# Assert original file is written
LET $o: STRING = PATH_TYPE("scratch.txt")
ASSERT_EQ $o "file"

REMOVE scratch.txt

# Assert file has been removed
LET $t: STRING = PATH_TYPE("scratch.txt")
ASSERT_EQ $t "absent"
```


### EXPAND

Expand a template file (or stdin) to stdout.

**Syntax:** `EXPAND [<path>] [<KEY=val> ...]`

A template is any text file — or piped stdin when no path is given —
containing `{{ ... }}` placeholders. EXPAND replaces each placeholder
and prints the result to stdout.

Placeholders: `{{ NAME }}` reads a `KEY=val` override passed on this
command; `{{ env:NAME }}` reads an override, falling back to the
environment; `{{ $var }}` reads a script variable (dotted paths allowed).
A missing key is an error, never a silent empty.

Calls: `{{ MODULE::FUNC(args) }}` invokes a pure registry function
with `$var.path` references and `STRING`, `INT`, `FLOAT`, and `BOOL`
literals as arguments. Only pure functions resolve here: stateful,
script-defined, and pipe-backed entries fail naming themselves,
since placeholder evaluation carries no step context. The same
calls resolve in ordinary strings, so both paths agree.

Substitution runs in a single pass. `EXPAND` is not recursive and does not
expand nested placeholders: a value that itself contains `{{ ... }}` is
inserted verbatim and never expanded again.

A bare `$var` argument is a template path; `KEY=val` arguments are
overrides whose values follow the unified string-value rules (same as
`ENV`: quotes keep exact bytes, a lone `$var` evaluates,
`{{ ... }}` interpolates).

NOTE: `WRITE` interpolates `{{ ... }}` while writing, so escape it
(`\{{ ... }}`) when writing a template file for a later `EXPAND`.

With no path, the template arrives on stdin through a pipe. When piping
from a shell, single-quote the template (`echo '{{ $x }}'`): double
quotes let the shell swallow `$x`, so oxdock receives an empty `{{ }}`
placeholder and errors.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | no | Template file to expand; omit to expand stdin |
| `overrides` | [`STRING...`](#value-type-string) | no | Template overrides shadowing that key (unified string values) |

**Output:** Stdout

**Examples:**

**Example: expand**

```oxdock
# Placeholders read overrides first, then the environment.
ENV NAME="Alice"
WRITE template.md "Hello {{ env:NAME }}!"
EXPAND template.md

ASSERT_CONTAINS stdout "Hello Alice!"
```

**Example: override with spaces**

```oxdock
# WRITE would interpolate {{ }} right away, so escape it.
# The file must literally contain {{ env:NAME }} for EXPAND.
WRITE template.md "Hello \{{ env:NAME }}!"
EXPAND template.md NAME="Alice Smith"

ASSERT_CONTAINS stdout "Hello Alice Smith!"
```

**Example: variable override**

```oxdock
# Same escaping: keep the placeholder literal until EXPAND.
# A lone $who evaluates, like ECHO $who.
LET $who: STRING = "Bob"
WRITE template.md "Hi \{{ env:WHO }}!"
EXPAND template.md WHO=$who

ASSERT_CONTAINS stdout "Hi Bob!"
```

**Example: override forms agree**

```oxdock
# A bare variable and a template-with-tail expand identically.
LET $x: STRING = "Ada"
WRITE template.md "Hi \{{ env:NAME }} and \{{ env:NAME2 }}!"
EXPAND template.md NAME=$x NAME2="{{ $x }} concatenated"

ASSERT_CONTAINS stdout "Hi Ada and Ada concatenated!"
```

**Example: expand stdin**

```oxdock
# No path: the template arrives on stdin through a pipe.
LET $tpl: PIPE
WITH_IO [stdout=$tpl] ECHO "Hello \{{ env:NAME }}!"
WITH_IO [stdin=$tpl] EXPAND NAME=Alice

ASSERT_CONTAINS stdout "Hello Alice!"
```

**Example: override does not leak**

```oxdock
# KEY=val overrides shadow env for that EXPAND only.
# They never update the environment itself.
ENV NAME="Alice"
WRITE template.md "Hi \{{ env:NAME }}!"

EXPAND template.md NAME="Bob"
ASSERT_CONTAINS stdout "Hi Bob!"

EXPAND template.md
ASSERT_CONTAINS stdout "Hi Alice!"
```

**Example: function call**

```oxdock
# Pure registry functions run inside placeholders.
# WRITE would interpolate {{ }} right away, so escape it.
IMPORT [STD]
WRITE template.md "Count: \{{ STD::TEXT_COUNT(\"one two two three\", \"two\") }}!"

EXPAND template.md
ASSERT_CONTAINS stdout "Count: 2!"
```


### ASSERT_EQ

Assert strict equality.

**Syntax:** `ASSERT_EQ <actual> <expected> | ASSERT_EQ --hash <sha256> <actual>`

Compares two evaluated values with typed equality (no coercion:
`INT(42)` never equals `STRING("42")`), aborting the pipeline
with a step-numbered error showing expected vs actual otherwise.

Both sides are values: `$var`, literals, templates, and calls
evaluate in memory and never touch disk. Read files explicitly
first (`LET $text: STRING = READ "out.txt"`, then
`ASSERT_EQ $text ...`).
Bare `stdout` / `stderr` observe stream buffers; a `$var`
holding a `PIPE` observes its backend bytes. `--hash` compares
the SHA-256 of a string, pipe, or captured-stdout actual
instead of the raw bytes (`stderr` is unsupported).


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `actual` | `<any>` | yes | Value, stdout, stderr, or a $var holding a PIPE |
| `expected` | `<any>...` | no | Expected (required unless --hash) |

**Flags:**

| Flag | Type | Description |
| --- | --- | --- |
| `--hash` | `STRING` | SHA-256 |

**Examples:**

**Example: assert eq**

```oxdock
LET $status: INT = 200
ASSERT_EQ $status 200
```

**Example: assert eq file**

```oxdock
WRITE payload.bin stable-content
LET $body: STRING = READ payload.bin
ASSERT_EQ $body "stable-content"
```

**Example: assert eq hash**

```oxdock
# --hash compares the SHA-256 digest instead of raw bytes.
WRITE payload.bin stable-content
LET $body: STRING = READ payload.bin
ASSERT_EQ --hash 08135c1b6349b0e4f894c36221952f0de00e6b4d82f80895abf359755e77103c $body
```


### ASSERT_CONTAINS

Assert containment.

**Syntax:** `ASSERT_CONTAINS <haystack> <needle>`

Checks containment and aborts the pipeline with a step-numbered
error otherwise: substring for strings, element match for lists,
key presence for maps, substring over stream and pipe buffers.

Like `ASSERT_EQ`, both sides are values read without implicit
I/O; read files explicitly first
(`LET $text: STRING = READ "cfg.txt"`).
Bare `stdout` / `stderr` observe stream buffers; a `$var`
holding a `PIPE` observes its backend bytes.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `haystack` | `<any>` | yes | Value, stdout, stderr, or a $var holding a PIPE |
| `needle` | `<any>...` | yes | Substring, element, or key |

**Examples:**

**Example: assert contains**

```oxdock
ECHO build-complete
ASSERT_CONTAINS stdout "build-complete"
```


### HASH_SHA256

Print SHA-256.

**Syntax:** `HASH_SHA256 <path>`

Computes digest.

**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | [`PATH`](#value-type-path) | yes | File |

**Output:** Stdout

**Examples:**

**Example: hash**

```oxdock
WRITE payload.txt hello
HASH_SHA256 payload.txt

LET $digest: STRING = HASH_SHA256 payload.txt
ASSERT_EQ $digest "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\n"
```


### EXIT

Exit pipeline.

**Syntax:** `EXIT <code>`

Stops the pipeline immediately with an `EXIT requested with code <code>`
error; steps after it never run, at any nesting depth.

Enclosing blocks still unwind their LET/ENV/WORKDIR/WORKSPACE state,
anonymous background tasks are killed synchronously, and files written
before the EXIT persist.

The code reaches the OS: the CLI exits with `<code>` clamped to
0..=255, and a guest EXIT relays through `REMOTE` the same way.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | [`INT`](#value-type-int) | yes | Code |

**Examples:**

**Example: exit**

```oxdock expect_error:"EXIT requested with code 0"
EXIT 0
```

**Expected error:** `EXIT requested with code 0`


### SLEEP

Pause execution for a duration.

**Syntax:** `SLEEP <duration>`

Parks the step for the duration (e.g. 500ms, 10s, 2m).

Cooperative: checks for cancellation so an enclosing TIMEOUT or task
teardown interrupts the sleep. Cross-platform alternative to shell sleep
for testing time boundaries.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `duration` | [`DURATION`](#value-type-duration) | yes | How long to sleep |

**Examples:**

**Example: sleep**

```oxdock
SLEEP 100ms
```

**Example: sleep variable duration**

```oxdock
# Durations resolve at runtime, so variables work too:
# quoted or bare, both bind the same string.
LET $pause: STRING = "100ms"
SLEEP $pause

LET $bare: STRING = 100ms
SLEEP $bare
```


### LIST_APPEND

Append an item to a LIST variable in place.

**Syntax:** `LIST_APPEND $list <item>`

Appends the item to the LIST variable in place.

When the binding holds the only reference the push runs in
amortized constant time. Aliased buffers detach first, so
other holders keep their contents.


**Arguments:**

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `list` | [`LIST`](#value-type-list) | yes | Target LIST variable (`$name`) |
| `item` | `<any>` | yes | Item to append (any value) |

**Examples:**

**Example: list append**

```oxdock
# Appends accumulate in order.
LET $items: LIST<STRING> = []
LIST_APPEND $items "first"
LIST_APPEND $items "second"

LET $want: LIST<STRING> = ["first", "second"]
ASSERT_EQ $items $want
```

## Value types

### Value type: INT

64-bit signed integer, e.g. an exit code.

### Value type: FLOAT

64-bit float, e.g. a ratio.

### Value type: STRING

Arbitrary text. Quotes keep exact bytes, lone `$var` evaluates, `{{ ... }}` interpolates.

### Value type: BOOL

Boolean `true` or `false`.

### Value type: LIST

Ordered list of values. Shared heap: cloning bumps a refcount.

### Value type: MAP

String-keyed map of values. Shared heap: cloning bumps a refcount.

### Value type: PATH

Workspace path, resolved against cwd and guarded against escape.

### Value type: DURATION

Positive time span: `500ms`, `10s`, `2m`, `1h`; bare number means seconds.

### Value type: PIPE

Anonymous pipe handle. The backend materializes lazily on first
binding (never eagerly at declaration), so the choice always has full
usage context. Cloning shares the backend (explicit-sharing fan-out);
equality is handle identity, never byte comparison.

### Value type: HANDLE

Background ASYNC task handle for AWAIT/CANCEL.

### Value type: SEMAPHORE

Counting semaphore for admission control. Cloning shares the backend
(explicit-sharing fan-out); equality is handle identity, never the
count.

### Value type: PERMIT

Opaque admission permit minted by `SEMAPHORE_TRY_ACQUIRE`. Cloning
shares the release obligation (first drops release nothing, the last
releases once); equality is handle identity.



<!-- GENERATED by docs-gen from STD function metadata. Do not edit by hand. -->
## Functions

Callable as `MODULE::NAME(...)` in expressions (or bare `NAME(...)` with the module imported via `IMPORT`). Introspectable from scripts with `FUNCTIONS()` and `DESCRIBE(name)`.

### STD::BASE64_DECODE

Decode standard base64 to a STRING.

Fails naming the input on invalid alphabet characters or
non-UTF-8 payloads instead of rendering a silent empty. Pairs
with `BASE64_ENCODE` for the round trip.


**Signature:** `STD::BASE64_DECODE($text: STRING) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Base64 text to decode.

**Returns:** `STRING`

**Examples:**

**Example: decode**

```oxdock
# Decode standard base64 back to text.
IMPORT [STD]
LET $decoded: STRING = BASE64_DECODE("aGVsbG8=")
ASSERT_EQ $decoded "hello"
```

### STD::BASE64_ENCODE

Encode a STRING as standard base64.

Padded RFC 4648 alphabet, so encoded output pastes into any
standard decoder. Pairs with `BASE64_DECODE` for the round trip.


**Signature:** `STD::BASE64_ENCODE($text: STRING) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to encode.

**Returns:** `STRING`

**Examples:**

**Example: encode**

```oxdock
# Encode text to standard base64.
IMPORT [STD]
LET $encoded: STRING = BASE64_ENCODE("hello")
ASSERT_EQ $encoded "aGVsbG8="
```

### STD::DESCRIBE

Describe one function by qualified name.

Returns a MAP with name, module, kind, params, returns, rpn, and
summary. `params` is always a LIST (empty for parameterless
functions) of entries carrying name, param_type, allowed, docs,
options, and optional flags, so the shape holds for every entry. Bare names
fail closed: `DESCRIBE` requires the qualified form (except
`INSPECT`, which is syntax rather than a registry entry). Errors
on unknown function.

Field table for the `DESCRIBE` result bundle: one source for the
shape the host mints, so introspection, enforcement, and
documentation cannot drift apart.

**Signature:** `STD::DESCRIBE($name: STRING) -> MAP<name: STRING, module: STRING, kind: STRING, params: LIST<MAP<ANY>>, returns: STRING, rpn: BOOL, summary: STRING>`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$name` (`STRING`): Qualified function name (`MODULE::NAME`).

**Returns:** `MAP<name: STRING, module: STRING, kind: STRING, params: LIST<MAP<ANY>>, returns: STRING, rpn: BOOL, summary: STRING>`

  - `name` (`STRING`): Qualified function name.
  - `module` (`STRING`): Owning module.
  - `kind` (`STRING`): Origin label (`host` or `script`).
  - `params` (`LIST<MAP<ANY>>`): Parameter entries (empty for parameterless functions).
  - `returns` (`STRING`): Structural return shape (empty when untagged).
  - `rpn` (`BOOL`): True when callable in RPN position.
  - `summary` (`STRING`): One-line description.

**Examples:**

**Example: describe**

```oxdock
IMPORT [STD]
LET $info: MAP<ANY> = DESCRIBE("STD::INT")
ASSERT_EQ $info.name "STD::INT"
ASSERT_EQ $info.returns "INT"
```

### STD::EOF

True when a pipe sits at end of stream: closed with nothing buffered,
so the next `READ_LINE` would bind `""` via EOF rather than a line.
Live writers, pinned keepers, and buffered bytes all answer false.
Never blocks: a reader already blocked stays blocked, so branch on
`EOF` before reading, not after. OS pairs and unbound handles answer
best-effort (kernel bytes are invisible there; see `INSPECT`).
Non-pipe arguments bail at the boundary: the parameter declares
`PIPE`, so the extractor and the static pass reject them before
the body runs.


**Signature:** `STD::EOF($pipe: PIPE) -> BOOL`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$pipe` (`PIPE`): Pipe handle to query for end of stream.

**Returns:** `BOOL`

**Examples:**

**Example: drain to end**

```oxdock
# Capture two lines, then drain to end of stream with no sentinel line.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "one"
    ECHO "two"
}

# The loop exits on close, and the count proves both lines drained.
LET $n: INT = 0
WHILE !EOF($cap) {
    WITH_IO [stdin=$cap] READ_LINE $line
    $n = $n + 1
}
ASSERT_EQ $n 2
```

### STD::FLOAT

Convert a value to FLOAT.

Parses f64 (accepts int strings), bails on non-finite or non-numeric.


**Signature:** `STD::FLOAT($val: FLOAT|INT|STRING) -> FLOAT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$val` (`FLOAT|INT|STRING`): Value to convert to `FLOAT`.

**Returns:** `FLOAT`

**Examples:**

**Example: convert**

```oxdock
IMPORT [STD]
LET $f: FLOAT = FLOAT("1.5")
ASSERT_EQ $f 1.5
LET $whole: FLOAT = FLOAT(2)
ASSERT_EQ $whole 2.0
```

### STD::FORMAT_BYTE_COUNT

Format a byte count canonically (`500B`, `10KB`, `2MB`, `1GB`,
binary `10KIB` and friends): the inverse of `PARSE_BYTE_COUNT`, so
formatted values reparse exactly. Negative inputs fail.


**Signature:** `STD::FORMAT_BYTE_COUNT($bytes: INT) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$bytes` (`INT`): Byte count to format; negatives fail.

**Returns:** `STRING`

**Examples:**

**Example: format and round-trip**

```oxdock
# Byte counts render in the largest exact unit.
IMPORT [STD]
LET $pretty: STRING = FORMAT_BYTE_COUNT(1_048_576)
ASSERT_EQ $pretty "1MiB"

# The pair round-trips through the parser.
LET $back: INT = PARSE_BYTE_COUNT($pretty)
ASSERT_EQ $back 1_048_576
```

### STD::FORMAT_DURATION

Format millisecond durations canonically (`500ms`, `10s`, `2m`,
`1h`): the inverse of `PARSE_DURATION`, so formatted values
reparse exactly. Negative inputs fail.


**Signature:** `STD::FORMAT_DURATION($millis: INT) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$millis` (`INT`): Milliseconds to format; negatives fail.

**Returns:** `STRING`

**Examples:**

**Example: format and round-trip**

```oxdock
# Durations render in the largest exact unit.
IMPORT [STD]
LET $pretty: STRING = FORMAT_DURATION(90_000)
ASSERT_EQ $pretty "90s"

# The pair round-trips through the parser.
LET $back: INT = PARSE_DURATION($pretty)
ASSERT_EQ $back 90_000
```

### STD::FUNCTIONS

List all visible function names.

Sorted LIST of qualified `MODULE::NAME` entries: DSL-defined plus native
plus host-registered names.

**Signature:** `STD::FUNCTIONS() -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast) only

**Returns:** `LIST<STRING>`

**Examples:**

**Example: list**

```oxdock
IMPORT [STD]
LET $names: LIST<STRING> = FUNCTIONS()
ASSERT_CONTAINS $names "STD::INT"
```

### STD::GLOB

List workspace paths matching a glob pattern.

Sorted, root-relative LIST; empty on no match or `..` escape.


**Signature:** `STD::GLOB($pattern: STRING|PATH) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$pattern` (`STRING|PATH`): Glob pattern matched against workspace paths.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: match**

```oxdock
IMPORT [STD]
WRITE "notes-a.md" "a"
WRITE "notes-b.md" "b"
WRITE "other.txt" "c"
LET $found: LIST<STRING> = GLOB("notes-*.md")
ASSERT_EQ $found.0 "notes-a.md"
ASSERT_EQ $found.1 "notes-b.md"
```

### STD::HAS_KEY

Report whether a map holds a key.

Pure MAP probe so scripts can branch on optional fields without
tripping the strict missing-key error.


**Signature:** `STD::HAS_KEY($map: MAP<ANY>, $key: STRING) -> BOOL`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$map` (`MAP<ANY>`): Map to probe.
- `$key` (`STRING`): Key to look up.

**Returns:** `BOOL`

**Examples:**

**Example: probe**

```oxdock
IMPORT [STD]
LET $m: MAP<ANY> = PARSE_JSON("{\"a\": 1}")
LET $hit: BOOL = HAS_KEY($m, "a")
ASSERT_EQ $hit true
LET $miss: BOOL = HAS_KEY($m, "b")
ASSERT_EQ $miss false
```

### STD::INT

Convert a value to INT.

Trims ASCII whitespace and parses i64. Passes Int through; Float only
when integral and finite; Bool as 1/0.


**Signature:** `STD::INT($val: INT|FLOAT|STRING|BOOL) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$val` (`INT|FLOAT|STRING|BOOL`): Value to convert to `INT`.

**Returns:** `INT`

**Examples:**

**Example: convert**

```oxdock
IMPORT [STD]
LET $n: INT = INT("42")
ASSERT_EQ $n 42
LET $flag: INT = INT(true)
ASSERT_EQ $flag 1
```

### STD::IS_EXEC

Report whether a name resolves to an executable file (#100).

Bare command names search `env:PATH` (`IS_EXEC("tar")` finds the
first executable `tar`, plus `tar.exe`, on `PATH`); spellings
containing a separator check that file directly. True means a real
file with an executable bit on Unix, or a `PATHEXT` extension on
Windows: each `env:PATHEXT` entry is probed in order (unset or empty
probes nothing). Absolute `PATH` entries resolve through the workspace as
usual, so host probing wants `WORKSPACE SYSTEM` and an inherited
`PATH`; anything unresolvable reads false, never errors. AST-only
by design, like `PATH_TYPE`: there is no RPN arm for filesystem IO.


**Signature:** `STD::IS_EXEC($name: STRING|PATH) -> BOOL`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$name` (`STRING|PATH`): Command name or file path to probe.

**Returns:** `BOOL`

**Examples:**

**Example: probe**

```oxdock
# Missing tools read false instead of failing the run.
IMPORT [STD]
LET $missing: BOOL = IS_EXEC("no-such-tool-xyz")
ASSERT_EQ $missing false

# Plain files are not executable on any platform.
WRITE tool.txt payload
LET $plain: BOOL = IS_EXEC("tool.txt")
ASSERT_EQ $plain false
```

### STD::IS_TERMINAL

Report whether a standard stream is a terminal.

`IS_TERMINAL("stdin")`, `IS_TERMINAL("stdout")`, or `IS_TERMINAL("stderr")`
answers for the step's stream as currently bound, so scripts can adapt
prompts, colors, and progress output. Anything diverted from the
terminal reports false without touching host handles: `WITH_IO` pipe
bindings (script backends and OS pairs), `LET`-capture sinks, staged
runner sinks, and any materialized stdin stream (only a directly
inherited fd falls back to the process check). A transparent root tee
still answers the session question via the process check. The name
matches exactly (no case folding): anything else bails. AST-only:
reads the step context like the other introspection functions.


**Signature:** `STD::IS_TERMINAL($stream_name: STRING = "stdin" | "stdout" | "stderr") -> BOOL`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$stream_name` (`STRING = "stdin" | "stdout" | "stderr"`): Stream name: `stdin`, `stdout`, or `stderr` (exact match).

**Returns:** `BOOL`

**Examples:**

**Example: null stdin**

```oxdock
# A null stdin is never a terminal, in every harness.
IMPORT [STD]
LET $term: BOOL = IS_TERMINAL("stdin")
ASSERT_EQ $term false
```

### STD::LOAD_JSON

Load and parse a JSON file.

Reads a workspace file and parses JSON into a DSL value. Returns
`ANY` by design: a top-level array or scalar parses to `LIST` or a
scalar word. `LET` coercion still checks the actual value at
assignment.


**Signature:** `STD::LOAD_JSON($path: STRING|PATH) -> ANY`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$path` (`STRING|PATH`): Workspace file path to load and parse as JSON.

**Returns:** `ANY`

**Examples:**

**Example: load**

```oxdock
IMPORT [STD]
WRITE "data.json" "{\"a\": 1}"
LET $doc: MAP<ANY> = LOAD_JSON("data.json")
ASSERT_EQ $doc.a 1
```

### STD::LOAD_TOML

Load and parse a TOML file.

Reads a workspace file and parses TOML into a DSL value.


**Signature:** `STD::LOAD_TOML($path: STRING|PATH) -> MAP<ANY>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$path` (`STRING|PATH`): Workspace file path to load and parse as TOML.

**Returns:** `MAP<ANY>`

**Examples:**

**Example: load**

```oxdock
IMPORT [STD]
WRITE "cfg.toml" "title = \"demo\""
LET $cfg: MAP<ANY> = LOAD_TOML("cfg.toml")
ASSERT_EQ $cfg.title "demo"
```

### STD::MAP_SET

Insert one key into a map.

Fails on duplicates so two entries sharing a key fail the run
instead of silently shadowing each other.


**Signature:** `STD::MAP_SET($map: MAP<ANY>, $key: STRING, $value: ANY) -> MAP<ANY>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$map` (`MAP<ANY>`): Map to insert into.
- `$key` (`STRING`): Key to insert; duplicates fail.
- `$value` (`ANY`): Value to store.

**Returns:** `MAP<ANY>`

**Examples:**

**Example: insert**

```oxdock
IMPORT [STD]
LET $m: MAP<ANY> = MAP_SET({seed: 0}, "a", 1)
ASSERT_EQ $m.a 1
```

### STD::MERGE_MAPS

Merge a LIST of MAPs in order under one duplicate policy.

`fail_on_duplicate` fails naming the repeated key, so two files
claiming one placeholder fail the run instead of shadowing each
other. `overwrite` lets later files win, for environment overlays.
Non MAP elements fail naming their position.


**Signature:** `STD::MERGE_MAPS($maps: LIST<MAP<ANY>>, $policy: STRING = "fail_on_duplicate" | "overwrite") -> MAP<ANY>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$maps` (`LIST<MAP<ANY>>`): LIST of MAPs to merge in order.
- `$policy` (`STRING = "fail_on_duplicate" | "overwrite"`): Duplicate policy.

**Returns:** `MAP<ANY>`

**Examples:**

**Example: merge**

```oxdock
IMPORT [STD]
LET $m: MAP<ANY> = MERGE_MAPS([{a: 1}, {b: 2}], "overwrite")
ASSERT_EQ $m.a 1
ASSERT_EQ $m.b 2
```

### STD::PARSE_BYTE_COUNT

Parse a byte-count token to `INT` bytes.

Bare digits are bytes; suffixes `B`/`KB`/`MB`/`GB`/`TB` scale
decimal and `KIB`/`MIB`/`GIB`/`TIB` binary, in any case. Plain
`INT` values pass through (already bytes); integral finite `FLOAT`
values convert. Pairs with `FORMAT_BYTE_COUNT` for the round trip.

```oxdock
# Units parse case-insensitively; bare digits are bytes.
IMPORT [STD]
LET $bytes: INT = PARSE_BYTE_COUNT("1Gb")
ASSERT_EQ $bytes 1_000_000_000

# Binary suffixes scale by powers of two.
LET $mebi: INT = PARSE_BYTE_COUNT("2mib")
ASSERT_EQ $mebi 2_097_152

# Digit runs group Rust-literal style.
LET $grouped: INT = PARSE_BYTE_COUNT("1_000_000")
ASSERT_EQ $grouped 1_000_000

# Plain numbers pass through as bytes already.
LET $plain: INT = PARSE_BYTE_COUNT(1_048_576)
ASSERT_EQ $plain 1_048_576

# Bare literals group too.
LET $bare: INT = PARSE_BYTE_COUNT(1_000_000)
ASSERT_EQ $bare 1_000_000

# One optional space separates number from unit.
LET $spaced: INT = PARSE_BYTE_COUNT("1 GB")
ASSERT_EQ $spaced 1_000_000_000
```


**Signature:** `STD::PARSE_BYTE_COUNT($text: INT|FLOAT|STRING) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`INT|FLOAT|STRING`): Byte-count token to parse, or bytes already.

**Returns:** `INT`

**Examples:**

**Example: parse**

```oxdock
IMPORT [STD]
LET $n: INT = PARSE_BYTE_COUNT("10KB")
ASSERT_EQ $n 10_000
LET $identity: INT = PARSE_BYTE_COUNT(512)
ASSERT_EQ $identity 512
```

### STD::PARSE_DURATION

Parse a duration token to `INT` milliseconds.

Suffixes `ms`/`s`/`m`/`h`; a bare number means seconds. Pairs with
`FORMAT_DURATION` for the round trip.


**Signature:** `STD::PARSE_DURATION($text: STRING) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Duration token to parse.

**Returns:** `INT`

**Examples:**

**Example: parse**

```oxdock
# A bare number means seconds, like TIMEOUT.
IMPORT [STD]
LET $ms: INT = PARSE_DURATION("2m")
ASSERT_EQ $ms 120_000

# Milliseconds stay exact.
LET $exact: INT = PARSE_DURATION("1500ms")
ASSERT_EQ $exact 1_500

# One optional space separates number from unit.
LET $spaced: INT = PARSE_DURATION("1 h")
ASSERT_EQ $spaced 3_600_000
```

### STD::PARSE_JSON

Parse JSON text already held in memory.

Uses the same conversion as file loading, so fetch bodies, file
contents, and captured text share one JSON value shape. Returns
`ANY` by design, matching `LOAD_JSON`: a top-level array or scalar
parses to `LIST` or a scalar word. `LET` coercion still checks the
actual value at assignment.


**Signature:** `STD::PARSE_JSON($text: STRING) -> ANY`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): JSON text already held in memory.

**Returns:** `ANY`

**Examples:**

**Example: parse**

```oxdock
# Parse in-memory JSON text with the same conversion as file loading.
IMPORT [STD]
LET $body: STRING = "{\"a\": 1}"
LET $doc: ANY = PARSE_JSON($body)
ASSERT_EQ $doc.a 1

# Top-level arrays parse to LIST, which is why the return is ANY.
LET $items: LIST<ANY> = PARSE_JSON("[1, 2]")
ASSERT_EQ $items.0 1
ASSERT_EQ $items.1 2
```

### STD::PARSE_TOML

Parse TOML text already held in memory.

Uses the same conversion as file loading, so fetch bodies, file
contents, and captured text share one JSON/TOML shape.


**Signature:** `STD::PARSE_TOML($text: STRING) -> MAP<ANY>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): TOML text already held in memory.

**Returns:** `MAP<ANY>`

**Examples:**

**Example: parse**

```oxdock
IMPORT [STD]
LET $doc: MAP<ANY> = PARSE_TOML("title = \"demo\"")
ASSERT_EQ $doc.title "demo"
```

### STD::PATH_JOIN

Join a directory and a file name with the host-native separator.

Both inputs accept `STRING` or workspace `PATH` words; the result
is a `STRING` joined with the platform separator, so installers
never hand-roll separators in templates. No filesystem access:
missing parents fail later at the consumer. Stays `STRING` (not
`PATH`) on purpose: the joined value is a not-yet-validated
destination string like `$dir` itself, and `PATH` words
re-canonicalize to forward slashes on template expansion, which
would undo the native join.

**Signature:** `STD::PATH_JOIN($dir: STRING|PATH, $name: STRING|PATH) -> STRING`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$dir` (`STRING|PATH`): Directory half of the destination.
- `$name` (`STRING|PATH`): File half of the destination.

**Returns:** `STRING`

**Examples:**

**Example: join path**

```oxdock
# Directory and file stem join with the native separator.
# Source `\` is one backslash at runtime; `/` works on Windows
# too, native just reads better there.
IMPORT [STD]
LET $p: STRING = PATH_JOIN("some-dir", "some-file")
[family:unix] ASSERT_EQ $p "some-dir/some-file"
[family:windows] ASSERT_EQ $p "some-dir\some-file"
```

### STD::PATH_TYPE

Describe a filesystem entry.

Reports file, dir, symlink (no-follow), or absent. AST-only by design;
there is no RPN arm for filesystem IO.


**Signature:** `STD::PATH_TYPE($path: STRING|PATH) -> STRING`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$path` (`STRING|PATH`): Workspace path of the entry to describe.

**Returns:** `STRING`

**Examples:**

**Example: describe**

```oxdock
IMPORT [STD]
WRITE "entry.txt" "words"
LET $kind: STRING = PATH_TYPE("entry.txt")
ASSERT_EQ $kind "file"
LET $missing: STRING = PATH_TYPE("no-such-entry.txt")
ASSERT_EQ $missing "absent"
```

### STD::PATH_WRITABLE

Report whether an ordinary file can be created at a path.

Walks up to the nearest existing ancestor, requires it be a
directory, and probes writability by creating and deleting a probe
file, so retry loops never litter. The empty string is false.
AST-only by design, like `PATH_TYPE`: there is no RPN arm for
filesystem IO.


**Signature:** `STD::PATH_WRITABLE($path: STRING|PATH) -> BOOL`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$path` (`STRING|PATH`): Workspace path to probe for file creation.

**Returns:** `BOOL`

**Examples:**

**Example: probe**

```oxdock
# An absent nested path is writable when its ancestor is.
IMPORT [STD]
LET $fresh: BOOL = PATH_WRITABLE("probe-check/nested/file.txt")
ASSERT_EQ $fresh true

# The empty string never aliases the working directory.
LET $empty: BOOL = PATH_WRITABLE("")
ASSERT_EQ $empty false
```

### STD::RAND

Draw a random FLOAT uniformly from [0, 1), optionally seeded.

Impure by design: unseeded calls return different values, so this
runs on the AST script path only, never as a `{{ }}` placeholder
call. A seed draws the same value on every run, for deterministic
fixtures. Scale and shift the draw for wider ranges.


**Signature:** `STD::RAND($options?: MAP<seed?: INT>) -> FLOAT`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$options?` (`MAP<seed?: INT>`): Draw settings: `seed`. Omittable: a missing options MAP fills `{}`.
  - `seed` (`INT`, optional)

**Returns:** `FLOAT`

**Examples:**

**Example: seeded draw**

```oxdock
# A seed draws the same value on every run.
IMPORT [STD]
LET $one: FLOAT = RAND({seed: 7})
LET $two: FLOAT = RAND({seed: 7})
ASSERT_EQ $one $two
```

**Example: unseeded draw**

```oxdock
# Scale a unit draw into a backoff delay between 1 and 11 seconds.
IMPORT [STD]
LET $draw: FLOAT = RAND()
LET $delay: FLOAT = 1.0 + $draw * 10.0
IF $delay < 1.0 {
    EXIT 1
}
IF $delay >= 11.0 {
    EXIT 1
}
ECHO "retrying in {{ $delay }}s"
```

### STD::SEMAPHORE_AVAILABLE

Read free permits under the lock, with no mutation.

Observability only (audit lines, healthchecks: `active = max - free`).
Exact at read time and stale the instant the caller acts on it, so it
must never drive admission: that is `SEMAPHORE_TRY_ACQUIRE`'s job.


**Signature:** `STD::SEMAPHORE_AVAILABLE($sem: SEMAPHORE) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$sem` (`SEMAPHORE`): Semaphore handle from `SEMAPHORE_NEW`.

**Returns:** `INT`

**Examples:**

**Example: readout**

```oxdock
# Hold one permit on a cap of two.
IMPORT [STD]
LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)
LET $acq: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
ASSERT_EQ $acq.held true

# One held permit leaves exactly one free.
LET $free: INT = SEMAPHORE_AVAILABLE($sem)
ASSERT_EQ $free 1
```

### STD::SEMAPHORE_NEW

Create a counting semaphore admitting at most `max` concurrent holders.

Non-positive maxima bail. The word names a shared backend: every clone
observes the same count, and admission runs through
`SEMAPHORE_TRY_ACQUIRE`, never through the `SEMAPHORE_AVAILABLE`
readout.


**Signature:** `STD::SEMAPHORE_NEW($max: INT) -> SEMAPHORE`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$max` (`INT`): Maximum concurrent holders; must be positive.

**Returns:** `SEMAPHORE`

**Examples:**

**Example: mint**

```oxdock
# Mint a semaphore admitting at most two holders.
IMPORT [STD]
LET $sem: SEMAPHORE = SEMAPHORE_NEW(2)

# A fresh semaphore reports its full cap as available.
LET $free: INT = SEMAPHORE_AVAILABLE($sem)
ASSERT_EQ $free 2
```

### STD::SEMAPHORE_TRY_ACQUIRE

Attempt one non-blocking acquire, always answering a MAP.

`held` is `true` with the permit under the `permit` key, or `false`
with no `permit` key: branch on `$m.held` directly. The DSL has no
null, so the absent key is the miss shape. Do not read `$m.permit`
unless `held`: missing-key access bails strictly.
Never waits, so no wait can wedge.


**Signature:** `STD::SEMAPHORE_TRY_ACQUIRE($sem: SEMAPHORE) -> MAP<held: BOOL, permit?: PERMIT>`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$sem` (`SEMAPHORE`): Semaphore handle from `SEMAPHORE_NEW`.

**Returns:** `MAP<held: BOOL, permit?: PERMIT>`

  - `held` (`BOOL`): `true` with the permit under `permit`, `false` with no `permit` key.
  - `permit` (`PERMIT`, optional): Permit handle; read only when `held`.

**Examples:**

**Example: acquire**

```oxdock
# First acquire holds with a permit to bind.
IMPORT [STD]
LET $sem: SEMAPHORE = SEMAPHORE_NEW(1)
LET $first: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
ASSERT_EQ $first.held true
LET $permit: PERMIT = $first.permit

# A capped semaphore answers held false with no permit key.
LET $second: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
ASSERT_EQ $second.held false
LET $has_permit: BOOL = HAS_KEY($second, "permit")
ASSERT_EQ $has_permit false
```

### STD::SHELL_JOIN_ARGS

Join argv words into one shell-parseable string.

Quotes each word with POSIX single-quote escaping through the
`shell-words` crate, so the result re-splits to the same words.
Inverse of `SHELL_SPLIT_ARGS` on the word side: splitting the
result returns the input list unchanged.


**Signature:** `STD::SHELL_JOIN_ARGS($words: LIST<STRING>) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$words` (`LIST<STRING>`): Words to join into a shell command string.

**Returns:** `STRING`

**Examples:**

**Example: join args**

```oxdock
# Plain words join with spaces and no quoting.
IMPORT [STD]
LET $plain: STRING = SHELL_JOIN_ARGS(["gh", "issue", "list"])
ASSERT_EQ $plain "gh issue list"

# Words with spaces quote, and the round trip is exact.
LET $words: LIST<STRING> = ["say", "hi there", "a'b"]
LET $joined: STRING = SHELL_JOIN_ARGS($words)
LET $back: LIST<STRING> = SHELL_SPLIT_ARGS($joined)
ASSERT_EQ $back.0 "say"
ASSERT_EQ $back.1 "hi there"
ASSERT_EQ $back.2 "a'b"
```

### STD::SHELL_SPLIT_ARGS

Split a shell command string into argv words.

Parses POSIX shell word syntax (single and double quotes, backslash
escapes) with the `shell-words` crate, the same splitter the remote
plugins use. Returns a `LIST<STRING>`. Unbalanced quotes fail.
Inverse of `SHELL_JOIN_ARGS`: joining the result re-splits
to the same words.

Shell syntax only: plain prose with no quoting splits on
whitespace like any shell would, which is rarely what prose wants.


**Signature:** `STD::SHELL_SPLIT_ARGS($command: STRING) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$command` (`STRING`): Shell command string to split into words.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: split args**

```oxdock
# Quoted segments stay one word.
IMPORT [STD]
LET $argv: LIST<STRING> = SHELL_SPLIT_ARGS("gh issue comment 153 --body-file notes.md")
ASSERT_EQ $argv.0 "gh"
ASSERT_EQ $argv.3 "153"
ASSERT_EQ $argv.5 "notes.md"

# Quotes and escapes parse the shell way.
LET $quoted: LIST<STRING> = SHELL_SPLIT_ARGS("say \"hi there\" 'a b'")
ASSERT_EQ $quoted.0 "say"
ASSERT_EQ $quoted.1 "hi there"
ASSERT_EQ $quoted.2 "a b"
```

### STD::TASK_POLL

Poll a background task without consuming it.

Reports the running state, the output tails accumulated so far,
and the final outcome once finished. Repeated polls are safe, and
a later `AWAIT` still joins normally. Polling after `AWAIT` fails
like double-await: the handle is spent.

Map keys: `state` always names the phase (`"running"` or
`"finished"`); `ok` is true unless the task failed; `stdout_tail`
and `stderr_tail` carry bounded output with `truncated_bytes`
counting dropped head bytes. Finished tasks add `exit_code`;
clean finishes add `result`, failed ones add `error` instead.

**Signature:** `STD::TASK_POLL($task: HANDLE) -> MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING>`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$task` (`HANDLE`): Task handle from ASYNC.

**Returns:** `MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING>`

  - `state` (`STRING`): Task phase: `running` or `finished`.
  - `ok` (`BOOL`): True unless the task failed.
  - `stdout_tail` (`STRING`): Bounded stdout tail, live while running.
  - `stderr_tail` (`STRING`): Bounded stderr tail, live while running.
  - `truncated_bytes` (`INT`): Dropped head bytes past the per-stream cap.
  - `exit_code` (`INT`, optional): Process exit code, present once finished.
  - `result` (`ANY`, optional): Task `RETURN` value, present on clean finish.
  - `error` (`STRING`, optional): Failure text, present on failed finish.

**Examples:**

**Example: running poll**

```oxdock
# A sleeping task is still running on the first poll.
IMPORT [STD]
LET $t: HANDLE = ASYNC { SLEEP 30s }
LET $m: MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING> = TASK_POLL($t)
ASSERT_EQ $m.state "running"
ASSERT_EQ $m.ok true
# Clean up the sleeper so the run ends promptly.
CANCEL $t
```

**Example: finished poll**

```oxdock
# A finished task reports its value and final tails, then still awaits.
IMPORT [STD]
LET $t: HANDLE = ASYNC { ECHO "hi"; SLEEP 1s; RETURN "done" }
# Do other work while the task runs; poll once it should be done.
SLEEP 2s
LET $m: MAP<state: STRING, ok: BOOL, stdout_tail: STRING, stderr_tail: STRING, truncated_bytes: INT, exit_code?: INT, result?: ANY, error?: STRING> = TASK_POLL($t)
ASSERT_EQ $m.state "finished"
ASSERT_EQ $m.result "done"
LET $o: STRING = AWAIT $t
ASSERT_EQ $o "done"
```

### STD::TEXT_CONTAINS

Report whether text holds a literal substring.

Returns `BOOL`; the substring search behind target filtering
alongside the prefix and suffix tests. Matching is
case-sensitive; there is no case-insensitive mode.


**Signature:** `STD::TEXT_CONTAINS($text: STRING, $needle: STRING) -> BOOL`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to search.
- `$needle` (`STRING`): Literal substring to look for.

**Returns:** `BOOL`

**Examples:**

**Example: contains**

```oxdock
# Present needles report true; misses report false.
IMPORT [STD]
LET $hit: BOOL = TEXT_CONTAINS("x86_64-linux-musl", "musl")
ASSERT_EQ $hit true

# A miss is false, not a failure.
LET $miss: BOOL = TEXT_CONTAINS("x86_64-linux-gnu", "musl")
ASSERT_EQ $miss false
```

### STD::TEXT_COUNT

Count non-overlapping literal occurrences of a needle.

Matches never share bytes, so `"aaa"` holds one `"aa"`, matching
`grep -o 'aa' | wc -l`. An empty needle fails closed instead of
binding every character boundary. Matching is case-sensitive;
there is no case-insensitive mode.


**Signature:** `STD::TEXT_COUNT($text: STRING, $needle: STRING) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to search.
- `$needle` (`STRING`): Literal needle to count; empty fails closed.

**Returns:** `INT`

**Examples:**

**Example: count**

```oxdock
# Every non-overlapping occurrence counts once.
IMPORT [STD]
LET $n: INT = TEXT_COUNT("hello world", "o")
ASSERT_EQ $n 2

# Overlapping candidates never double count.
LET $once: INT = TEXT_COUNT("aaa", "aa")
ASSERT_EQ $once 1
```

### STD::TEXT_COUNT_MATCHES

Count lines matching a regex pattern.

Lines split with universal newline handling, like `TEXT_LINES`,
and each line reports independently: this is `grep -c`, so a line
with two matches still counts once. Bad patterns fail closed
naming the pattern and its position. Matching is case-sensitive
unless the pattern carries an inline `(?i)` flag, which makes
uppercase and lowercase letters match each other.


**Signature:** `STD::TEXT_COUNT_MATCHES($text: STRING, $pattern: STRING) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text whose lines to test.
- `$pattern` (`STRING`): Regex pattern tested per line.

**Returns:** `INT`

**Examples:**

**Example: count matches**

```oxdock
# One line matches once no matter how many times it hits.
IMPORT [STD]
LET $n: INT = TEXT_COUNT_MATCHES("test a: test\ntest b: nope\ntest c: test", ": test")
ASSERT_EQ $n 2

# A miss binds zero, never an error.
LET $none: INT = TEXT_COUNT_MATCHES("test b: nope", ": test$")
ASSERT_EQ $none 0
```

### STD::TEXT_ENDS_WITH

Report whether text ends with a literal suffix.

Returns `BOOL`; the `endsWith(matrix.target, 'linux-musl')`
check in CI legs. Matching is case-sensitive; there is no
case-insensitive mode.


**Signature:** `STD::TEXT_ENDS_WITH($text: STRING, $suffix: STRING) -> BOOL`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to test.
- `$suffix` (`STRING`): Literal suffix to look for.

**Returns:** `BOOL`

**Examples:**

**Example: ends with**

```oxdock
# Present suffixes report true; misses report false.
IMPORT [STD]
LET $hit: BOOL = TEXT_ENDS_WITH("x86_64-linux-musl", "linux-musl")
ASSERT_EQ $hit true

# A miss is false, not a failure.
LET $miss: BOOL = TEXT_ENDS_WITH("x86_64-linux-gnu", "linux-musl")
ASSERT_EQ $miss false
```

### STD::TEXT_EXTRACT

Extract capture groups for the first regex match.

Returns a `LIST<STRING>` where index 0 is the full match and the
rest are groups in order; non-participating groups bind empty so
indexes stay stable. No match binds an empty list. Matching is
case-sensitive unless the pattern carries an inline `(?i)` flag,
which makes uppercase and lowercase letters match each other.


**Signature:** `STD::TEXT_EXTRACT($text: STRING, $pattern: STRING) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to search.
- `$pattern` (`STRING`): Regex pattern with capture groups.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: extract**

```oxdock
# Index zero is the full match; groups follow in order.
IMPORT [STD]
LET $caps: LIST<STRING> = TEXT_EXTRACT("abc123", "([a-z]+)(\d+)")
ASSERT_EQ $caps.0 "abc123"
ASSERT_EQ $caps.1 "abc"
ASSERT_EQ $caps.2 "123"
```

### STD::TEXT_FIELDS

Split text into whitespace-separated fields, ignoring empties.

Returns a `LIST<STRING>`; the checksum-line parser that replaces
`cut -d' ' -f1` in CI legs.


**Signature:** `STD::TEXT_FIELDS($text: STRING) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to split into fields.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: fields**

```oxdock
# Runs of whitespace separate fields; empties never appear.
IMPORT [STD]
LET $fields: LIST<STRING> = TEXT_FIELDS("  abc  def ")
ASSERT_EQ $fields.0 "abc"
ASSERT_EQ $fields.1 "def"
```

### STD::TEXT_GREP

Keep the lines matching a regex pattern.

Splits with universal newline handling, like `TEXT_LINES`, and
returns every matching line in order: plain `grep`, so a miss
binds an empty list instead of failing. A trailing options MAP
carries `invert`: `TEXT_GREP($text, $pattern, {invert: true})`
keeps every non-matching line instead, the `grep -v` shape. The
MAP omits freely; a missing MAP keeps matches. Matching is
case-sensitive unless the pattern carries an inline `(?i)` flag,
which makes uppercase and lowercase letters match each other. Bad
patterns fail closed naming the pattern and its position.


**Signature:** `STD::TEXT_GREP($text: STRING, $pattern: STRING, $options?: MAP<invert?: BOOL>) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text whose lines to filter.
- `$pattern` (`STRING`): Regex pattern keeping a line.
- `$options?` (`MAP<invert?: BOOL>`): Filter settings: `invert`. Omittable: a missing options MAP fills `{}`.
  - `invert` (`BOOL`, optional)

**Returns:** `LIST<STRING>`

**Examples:**

**Example: grep**

```oxdock
# Matching lines come back as a list, in order.
IMPORT [STD]
LET $hits: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one")
ASSERT_EQ $hits.0 "one"
ASSERT_EQ $hits.1 "one more"

# A miss binds an empty list, never an error.
LET $miss: LIST<STRING> = TEXT_GREP("one\ntwo", "^z")
LET $joined: STRING = TEXT_JOIN($miss, ",")
ASSERT_EQ $joined ""

# The (?i) flag ignores case, so uppercase lines match too.
LET $folded: LIST<STRING> = TEXT_GREP("One\ntwo\nONE more", "(?i)^one")
ASSERT_EQ $folded.0 "One"
ASSERT_EQ $folded.1 "ONE more"
```

**Example: grep invert**

```oxdock
# The invert option keeps every non-matching line instead.
IMPORT [STD]
LET $kept: LIST<STRING> = TEXT_GREP("one\ntwo\none more", "^one", {invert: true})
ASSERT_EQ $kept.0 "two"

# A pattern matching every line binds an empty list, never an error.
LET $none: LIST<STRING> = TEXT_GREP("one\ntwo", "^", {invert: true})
LET $joined: STRING = TEXT_JOIN($none, ",")
ASSERT_EQ $joined ""

# Options ride into placeholders as MAP literals. Placeholder
# strings unescape only backslash and quote, so multiline text
# arrives through a variable.
LET $two_lines: STRING = "one\ntwo"
LET $msg: STRING = 'kept:{{ STD::TEXT_GREP($two_lines, "^one", {invert: true}) }}'
ASSERT_EQ $msg "kept:two"
```

### STD::TEXT_JOIN

Join a `LIST<STRING>` with a literal separator into a `STRING`.


**Signature:** `STD::TEXT_JOIN($elements: LIST<STRING>, $delimiter: STRING) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$elements` (`LIST<STRING>`): List elements to join.
- `$delimiter` (`STRING`): Literal delimiter placed between elements.

**Returns:** `STRING`

**Examples:**

**Example: join**

```oxdock
# List items join with the separator between them.
IMPORT [STD]
LET $joined: STRING = TEXT_JOIN(["a", "b", "c"], ",")
ASSERT_EQ $joined "a,b,c"
```

### STD::TEXT_LENGTH

Count the characters in text as Unicode scalars.

Multibyte characters count once each, so this is not the byte
length: `"héllo"` binds 5, not 6. The `${#var}` and `wc -m`
counterpart for scripts that size tags and digests.


**Signature:** `STD::TEXT_LENGTH($text: STRING) -> INT`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to measure.

**Returns:** `INT`

**Examples:**

**Example: length**

```oxdock
# Plain text counts one per character.
IMPORT [STD]
LET $n: INT = TEXT_LENGTH("hello")
ASSERT_EQ $n 5

# Multibyte characters count once each, not per byte.
LET $wide: INT = TEXT_LENGTH("héllo")
ASSERT_EQ $wide 5
```

### STD::TEXT_LINES

Split text into lines with universal newline handling.

Normalizes through the line-ending crate first so LF, CRLF, CR,
and mixed endings all split, then delegates the split itself to
`LineEnding::split` instead of hand-rolled scanning.
Returns a `LIST<STRING>`; eager-only like `TEXT_SPLIT`.


**Signature:** `STD::TEXT_LINES($text: STRING) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to split into lines.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: lines**

```oxdock
# Newlines split into line items.
IMPORT [STD]
LET $lines: LIST<STRING> = TEXT_LINES("one\ntwo\nthree")
ASSERT_EQ $lines.0 "one"
ASSERT_EQ $lines.1 "two"
ASSERT_EQ $lines.2 "three"

# Windows endings split the same way after normalization.
LET $crlf: LIST<STRING> = TEXT_LINES("one\r\ntwo\r\nthree")
ASSERT_EQ $crlf.0 "one"
ASSERT_EQ $crlf.1 "two"
ASSERT_EQ $crlf.2 "three"

# Old Mac endings split too, and mixed endings split together because the input normalizes to LF before splitting.
LET $mixed: LIST<STRING> = TEXT_LINES("a\r\nb\nc\rd")
ASSERT_EQ $mixed.0 "a"
ASSERT_EQ $mixed.1 "b"
ASSERT_EQ $mixed.2 "c"
ASSERT_EQ $mixed.3 "d"
```

### STD::TEXT_LINES_RANGE

Read lines `from` through `to` as a `LIST<STRING>`: the `sed -n
'A,Bp'` window over universal newlines, for reading one file
section without holding the surrounding text.

Lines are 1-based and inclusive on both ends. Ranges past the end
clamp like sed instead of failing; a start before line 1 or past
`to` fails closed, since no line reading satisfies it.


**Signature:** `STD::TEXT_LINES_RANGE($text: STRING, $from: INT, $to: INT) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text whose lines to window.
- `$from` (`INT`): First line, 1-based inclusive.
- `$to` (`INT`): Last line, inclusive; past the end clamps.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: line range**

```oxdock
# Lines 2 through 3 come back as a list, 1-based inclusive.
IMPORT [STD]
LET $window: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo\nthree\nfour", 2, 3)
ASSERT_EQ $window.0 "two"
ASSERT_EQ $window.1 "three"

# Ranges past the end clamp instead of failing.
LET $tail: LIST<STRING> = TEXT_LINES_RANGE("one\ntwo", 2, 9)
ASSERT_EQ $tail.0 "two"
```

### STD::TEXT_MATCH

Report whether text matches a regex pattern.

Returns `BOOL`. Bad patterns fail closed naming the pattern and
its position. Matching is case-sensitive unless the pattern
carries an inline `(?i)` flag, which makes uppercase and lowercase
letters match each other.


**Signature:** `STD::TEXT_MATCH($text: STRING, $pattern: STRING) -> BOOL`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to test.
- `$pattern` (`STRING`): Regex pattern to match.

**Returns:** `BOOL`

**Examples:**

**Example: match**

```oxdock
# Regex matches report true; misses report false.
IMPORT [STD]
LET $hit: BOOL = TEXT_MATCH("abc123", "\d+")
ASSERT_EQ $hit true

# A miss is false, not a failure.
LET $miss: BOOL = TEXT_MATCH("abc", "^\d+$")
ASSERT_EQ $miss false
```

### STD::TEXT_REPLACE

Replace every literal occurrence of `from` with `to`.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes scan through a `from.len()` sliding window, so needles split
across chunk reads still rewrite.
Literal sliding search, not a pattern: an empty `from` fails
closed, and regex work with captures belongs to
`TEXT_REPLACE_REGEX`. Matching is case-sensitive; there is no
case-insensitive mode.


**Signature:** `STD::TEXT_REPLACE($text: STRING|PIPE, $from: STRING, $to: STRING) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to rewrite.
- `$from` (`STRING`): Literal text to search for; empty fails closed.
- `$to` (`STRING`): Literal replacement text.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: replace**

```oxdock
# Every literal occurrence rewrites in place.
IMPORT [STD]
LET $fixed: STRING = TEXT_REPLACE("hello world", "world", "there")
ASSERT_EQ $fixed "hello there"
```

**Example: replace pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same rewrite.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "hello world"
}
LET $trimmed: PIPE = TEXT_TRIM($cap)
LET $out: PIPE = TEXT_REPLACE($trimmed, "world", "there")
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "hello there"
```

**Example: replace template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'fixed:{{ STD::TEXT_REPLACE("hello world", "world", "there") }}'
ASSERT_EQ $msg "fixed:hello there"
```

### STD::TEXT_REPLACE_REGEX

Replace every regex match, expanding `$1` and `$name` captures.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Eager-only framing: matches span chunks, so pipe inputs stage
through the spill substrate instead of streaming.
Bad patterns fail closed naming the pattern and its position.
Matching is case-sensitive unless the pattern carries an inline
`(?i)` flag, which makes uppercase and lowercase letters match
each other.
The literal counterpart is `TEXT_REPLACE`.


**Signature:** `STD::TEXT_REPLACE_REGEX($text: STRING|PIPE, $pattern: STRING, $replacement: STRING) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to rewrite.
- `$pattern` (`STRING`): Regex pattern to match.
- `$replacement` (`STRING`): Replacement with `$1` and `$name` capture expansion.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: replace with captures**

```oxdock
# Capture groups expand in the replacement.
IMPORT [STD]
LET $swapped: STRING = TEXT_REPLACE_REGEX("2026-10-13", "(\d+)-(\d+)-(\d+)", "$3/$2/$1")
ASSERT_EQ $swapped "13/10/2026"
```

**Example: replace regex pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same rewrite.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "2026-10-13"
}
LET $trimmed: PIPE = TEXT_TRIM($cap)
LET $out: PIPE = TEXT_REPLACE_REGEX($trimmed, "(\d+)-(\d+)-(\d+)", "$3/$2/$1")
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "13/10/2026"
```

**Example: replace regex template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'swapped:{{ STD::TEXT_REPLACE_REGEX("2026-10-13", "(\d+)-(\d+)-(\d+)", "$3/$2/$1") }}'
ASSERT_EQ $msg "swapped:13/10/2026"
```

### STD::TEXT_SPLIT

Split text on a literal delimiter into a `LIST<STRING>`.

An empty delimiter splits into single characters. Eager-only by
design: splitting into a list has no line framing to stream.
Delimiters match case-sensitively; there is no
case-insensitive mode.


**Signature:** `STD::TEXT_SPLIT($text: STRING, $delimiter: STRING) -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to split.
- `$delimiter` (`STRING`): Literal delimiter; empty splits into characters.

**Returns:** `LIST<STRING>`

**Examples:**

**Example: split**

```oxdock
# Delimiters split into list items.
IMPORT [STD]
LET $parts: LIST<STRING> = TEXT_SPLIT("a,b,c", ",")
ASSERT_EQ $parts.0 "a"
ASSERT_EQ $parts.1 "b"
ASSERT_EQ $parts.2 "c"
```

### STD::TEXT_STARTS_WITH

Report whether text starts with a literal prefix.

Returns `BOOL`; the literal check behind tag and target branching
such as `matrix.target` suffix and prefix tests. Matching is
case-sensitive; there is no case-insensitive mode.


**Signature:** `STD::TEXT_STARTS_WITH($text: STRING, $prefix: STRING) -> BOOL`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING`): Text to test.
- `$prefix` (`STRING`): Literal prefix to look for.

**Returns:** `BOOL`

**Examples:**

**Example: starts with**

```oxdock
# Present prefixes report true; misses report false.
IMPORT [STD]
LET $hit: BOOL = TEXT_STARTS_WITH("linux-musl-x64", "linux")
ASSERT_EQ $hit true

# A miss is false, not a failure.
LET $miss: BOOL = TEXT_STARTS_WITH("macos-arm", "linux")
ASSERT_EQ $miss false
```

### STD::TEXT_TO_LOWER

Lowercase text with full Unicode mapping.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes map chunk by chunk across a char-boundary carry, like
`TEXT_TO_UPPER`.


**Signature:** `STD::TEXT_TO_LOWER($text: STRING|PIPE) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to lowercase.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: lowercase**

```oxdock
# Text folds to lowercase.
IMPORT [STD]
LET $quiet: STRING = TEXT_TO_LOWER("HELLO")
ASSERT_EQ $quiet "hello"
```

**Example: lowercase pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same fold.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "HELLO"
}
LET $out: PIPE = TEXT_TO_LOWER($cap)
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "hello"
```

**Example: lowercase template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'quiet:{{ STD::TEXT_TO_LOWER("HELLO") }}'
ASSERT_EQ $msg "quiet:hello"
```

### STD::TEXT_TO_UPPER

Uppercase text with full Unicode mapping.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes map chunk by chunk across a char-boundary carry, so
multibyte characters split across reads still fold correctly.


**Signature:** `STD::TEXT_TO_UPPER($text: STRING|PIPE) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to uppercase.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: uppercase**

```oxdock
# Text folds to uppercase.
IMPORT [STD]
LET $loud: STRING = TEXT_TO_UPPER("hello")
ASSERT_EQ $loud "HELLO"
```

**Example: uppercase pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same fold.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "hello"
}
LET $out: PIPE = TEXT_TO_UPPER($cap)
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "HELLO"
```

**Example: uppercase template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'loud:{{ STD::TEXT_TO_UPPER("hello") }}'
ASSERT_EQ $msg "loud:HELLO"
```

### STD::TEXT_TRIM

Trim Unicode whitespace from both ends of text.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes stream on a pump thread with O(chunk) memory: leading
whitespace falls at the stream head while trailing whitespace
holds in the spill substrate until the next segment proves it
interior, so unbounded streams never buffer unbounded memory.


**Signature:** `STD::TEXT_TRIM($text: STRING|PIPE) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to trim.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: trim**

```oxdock
# Leading and trailing whitespace falls away.
IMPORT [STD]
LET $clean: STRING = TEXT_TRIM("  hello  ")
ASSERT_EQ $clean "hello"
```

**Example: trim pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same transform.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "  hello  "
}
LET $out: PIPE = TEXT_TRIM($cap)
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "hello"
```

**Example: trim template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'clean:{{ STD::TEXT_TRIM("  hello  ") }}'
ASSERT_EQ $msg "clean:hello"
```

**Example: trim file template**

```oxdock
# EXPAND templates resolve the same registry.
IMPORT [STD]
WRITE "tpl.txt" "clean:\{{ STD::TEXT_TRIM(\"  hello  \") }}"
LET $out: STRING = EXPAND "tpl.txt"
ASSERT_EQ $out "clean:hello"
```

### STD::TEXT_TRIM_PREFIX

Strip one leading prefix, returning the input unchanged when absent.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes decide on the first `prefix.len()` bytes and stream the rest
untouched. Matching is case-sensitive; there is no
case-insensitive mode.
Used for tag prefix strips such as `v1.2.3` to `1.2.3`.


**Signature:** `STD::TEXT_TRIM_PREFIX($text: STRING|PIPE, $prefix: STRING) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to strip.
- `$prefix` (`STRING`): Leading prefix to remove once.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: strip prefix**

```oxdock
# A present prefix falls away; absence returns the input unchanged.
IMPORT [STD]
LET $bare: STRING = TEXT_TRIM_PREFIX("v1.2.3", "v")
ASSERT_EQ $bare "1.2.3"

# Missing prefixes pass through untouched.
LET $same: STRING = TEXT_TRIM_PREFIX("1.2.3", "v")
ASSERT_EQ $same "1.2.3"
```

**Example: strip prefix pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same strip.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "v1.2.3"
}
LET $trimmed: PIPE = TEXT_TRIM($cap)
LET $out: PIPE = TEXT_TRIM_PREFIX($trimmed, "v")
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "1.2.3"
```

**Example: strip prefix template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_PREFIX("v1.2.3", "v") }}'
ASSERT_EQ $msg "bare:1.2.3"
```

### STD::TEXT_TRIM_SUFFIX

Strip one trailing suffix, returning the input unchanged when absent.

Accepts `STRING` or `PIPE`; the return mirrors the input kind.
Pipes hold a `suffix.len()` trailing window and stream the rest.
Matching is case-sensitive; there is no case-insensitive mode.


**Signature:** `STD::TEXT_TRIM_SUFFIX($text: STRING|PIPE, $suffix: STRING) -> STRING|PIPE`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$text` (`STRING|PIPE`): Text to strip.
- `$suffix` (`STRING`): Trailing suffix to remove once.

**Returns:** `STRING|PIPE`

**Examples:**

**Example: strip suffix**

```oxdock
# A present suffix falls away; absence returns the input unchanged.
IMPORT [STD]
LET $bare: STRING = TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz")
ASSERT_EQ $bare "archive.tar"

# Missing suffixes pass through untouched.
LET $same: STRING = TEXT_TRIM_SUFFIX("archive.tar", ".gz")
ASSERT_EQ $same "archive.tar"
```

**Example: strip suffix pipe mirrors**

```oxdock
# Pipe input mints pipe output with the same strip.
IMPORT [STD]
LET $cap: PIPE
WITH_IO [stdout=$cap] {
    ECHO "archive.tar.gz"
}
LET $trimmed: PIPE = TEXT_TRIM($cap)
LET $out: PIPE = TEXT_TRIM_SUFFIX($trimmed, ".gz")
WITH_IO [stdin=$out] READ_LINE $line
ASSERT_EQ $line "archive.tar"
```

**Example: strip suffix template**

```oxdock
# String inputs run inside placeholders too.
IMPORT [STD]
LET $msg: STRING = 'bare:{{ STD::TEXT_TRIM_SUFFIX("archive.tar.gz", ".gz") }}'
ASSERT_EQ $msg "bare:archive.tar"
```

### STD::TO_JSON

Encode a script value as JSON with one trailing newline.

Maps stay sorted; only template-safe shapes (STRING, INT, FLOAT,
BOOL, LIST, MAP) survive, anything else fails here instead of
rendering as a silent empty.


**Signature:** `STD::TO_JSON($value: STRING|INT|FLOAT|BOOL|LIST|MAP) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$value` (`STRING|INT|FLOAT|BOOL|LIST|MAP`): Value to encode as JSON.

**Returns:** `STRING`

**Examples:**

**Example: encode**

```oxdock
IMPORT [STD]
LET $json: STRING = TO_JSON({a: 1})
ASSERT_EQ $json "{\"a\":1}\n"
```

### STD::TYPES

List all known type names.

Sorted LIST of startup plus host-registered type descriptors. Reads the
run's name directory, so it runs on the AST path like the other
introspection functions.

**Signature:** `STD::TYPES() -> LIST<STRING>`

**Contexts:** [`AST`](#context-ast) only

**Returns:** `LIST<STRING>`

**Examples:**

**Example: list**

```oxdock
IMPORT [STD]
LET $names: LIST<STRING> = TYPES()
ASSERT_CONTAINS $names "STRING"
```

### STD::TYPE_DESCRIBE

Describe one type by name.

Returns a MAP with name, summary, and docs. Errors on unknown type.
Reads the run's name directory, so it runs on the AST path.


**Signature:** `STD::TYPE_DESCRIBE($name: STRING) -> MAP<name: STRING, summary: STRING, docs: STRING>`

**Contexts:** [`AST`](#context-ast) only

**Parameters:**

- `$name` (`STRING`): Type name to describe.

**Returns:** `MAP<name: STRING, summary: STRING, docs: STRING>`

  - `name` (`STRING`): Queried type name.
  - `summary` (`STRING`): One-line description.
  - `docs` (`STRING`): Full documentation text.

**Examples:**

**Example: describe**

```oxdock
IMPORT [STD]
LET $info: MAP<ANY> = TYPE_DESCRIBE("STRING")
ASSERT_EQ $info.name "STRING"
```

### STD::TYPE_OF

Name the word a value holds, for data-driven branching.

Returns the registered type word (`STRING`, `INT`, `FLOAT`, `BOOL`,
`LIST`, `MAP`, plus handle words like `PIPE`): the same name the
value prints in arity and coercion errors, so scripts can branch
on config shapes (a path string or a path list) without failing.


**Signature:** `STD::TYPE_OF($value: ANY) -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Parameters:**

- `$value` (`ANY`): Value whose word to name.

**Returns:** `STRING`

**Examples:**

**Example: name the word**

```oxdock
IMPORT [STD]
LET $word: STRING = TYPE_OF(42)
ASSERT_EQ $word "INT"
```

### STD::VERSION

Report the running engine version.

Returns the workspace version baked in at compile time, so scripts
and agents can branch on engine behavior instead of probing it.

**Signature:** `STD::VERSION() -> STRING`

**Contexts:** [`AST`](#context-ast), [`RPN`](#context-rpn)

**Returns:** `STRING`

**Examples:**

**Example: report**

```oxdock
# The engine reports its own build version as a dotted string.
IMPORT [STD]
LET $v: STRING = VERSION()
ASSERT_CONTAINS $v "."
```

<!-- GENERATED by docs-gen from function context metadata. Do not edit by hand. -->
## Contexts

Every function lists the contexts it runs in. `AST` is always available. `RPN` additionally requires a pure function or an explicit `rpn` opt in.

### Context: AST

The parsed tree of a script, walked one step at a time. Statements, declarations, scopes, pipes, and IO live here. Every step knows its line number, so failures name it. Every function runs in this context.

### Context: RPN

Arithmetic and comparison expressions compiled to a flat stack program of values with no statements, scopes, pipes, or step numbers. Pure functions run here with no flag. Stateful functions run here only when they opt in with `#[oxdock_func(rpn)]`, which is reserved for read only queries that stay meaningful inside math (`GLOB`, `LOAD_TOML`, `LOAD_JSON` do this). A call that fails inside math reports the bare error with no step number.

## Errors stop the pipeline

A pipeline is expected to run to completion. The engine is fail fast:
the first failed step ends the run immediately, later steps do not
execute, and the failure is reported with its step context. There is
no retry, resume, or continue on error mode, and partial completion is
not treated as success.

Choosing not to run work is different from failing it. Guards and
branches select work, timeouts bound it, and `EXIT <code>` is the
intentional form of an immediate stop. Files written before the stop
persist, while unwinding and task teardown follow the normal scope
rules.

### When a script fails to parse

Parse errors tell you what went wrong, where, and what was expected.
Every error names the line and column, echoes the source line, and points
a caret at the offending text. A syntax error is never reported as
`unknown command`: any line starting with a known command or keyword
always explains the real problem instead. Each example below runs the
real parser and pins its exact output.

A typo in the command name reports the line with a caret:

```rust
use indoc::indoc;
use oxdock::oxdock_parser;

let script = indoc! {"
    FROBNICATE hi
"};
let err = oxdock_parser::parse_script(script, oxdock_parser::lower_command)
    .expect_err("must fail");
let expected = indoc! {"
    unknown command: FROBNICATE
      --> line 1, col 1-13
      1 | FROBNICATE hi
        | ^^^^^^^^^^^^^
"};
assert_eq!(err.to_string(), expected.trim_end());
```

A malformed line for a real command reports the expected syntax with an
example. Here the `LET` is missing its required type:

```rust
use indoc::indoc;
use oxdock::oxdock_parser;

let script = indoc! {"
    LET $x = 1
"};
let err = oxdock_parser::parse_script(script, oxdock_parser::lower_command)
    .expect_err("must fail");
let expected = indoc! {"
    invalid syntax for command LET: LET assigns a variable, e.g. `LET $name: STRING = <expr>`, `LET $t: HANDLE = ASYNC ...`, `LET $out: STRING = <command>` (capture), `LET $out: STRING = AWAIT $t`, or `LET $var: TYPE = { ... }` (inline block); got `$x = 1`.
      --> line 1, col 1-10
      1 | LET $x = 1
        | ^^^^^^^^^^
"};
assert_eq!(err.to_string(), expected.trim_end());
```

A valid command with malformed `WITH_IO` bindings explains the binding
rules instead of claiming the command does not exist:

```rust
use indoc::indoc;
use oxdock::oxdock_parser;

let script = indoc! {"
    WITH_IO [stdout=discard] ECHO hi
"};
let err = oxdock_parser::parse_script(script, oxdock_parser::lower_command)
    .expect_err("must fail");
let expected = indoc! {"
    invalid syntax for command WITH_IO: WITH_IO needs `WITH_IO [bindings] <command>` or `WITH_IO [bindings] { <commands> }`: invalid binding `stdout=discard`; bindings are `stdin`, `stdout`, `stderr`, or `<stream>=$var` with a PIPE-typed variable (e.g. `[stdout=$p]`, `[stdin=$p]`); `tty` is a bare flag for RUN steps, with `rows = <int>` / `cols = <int>` geometry (e.g. `[tty, rows=40, cols=100]`); got `[stdout=discard] ECHO hi`.
      --> line 1, col 1-32
      1 | WITH_IO [stdout=discard] ECHO hi
        | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
"};
assert_eq!(err.to_string(), expected.trim_end());
```

Lowercase commands are caught by the grammar with a correction note:
```rust
use indoc::indoc;
use oxdock::oxdock_parser;

let script = indoc! {"
    echo hi
"};
let err = oxdock_parser::parse_script(script, oxdock_parser::lower_command)
    .expect_err("must fail");
let expected = indoc! {"
    parse error (expected: script)
    note: command must be uppercase: found `echo`, expected `ECHO`
      --> line 1, col 1-1
      1 | echo hi
        | ^
"};
assert_eq!(err.to_string(), expected.trim_end());
```

Reading an error, top to bottom: the first line states the kind and the
fix (expected syntax or a `did you mean` hint), the `-->` line gives the
exact position, and the caret marks the token to change. For block and
guard mistakes the caret covers the statement; for argument mistakes such
as a bad `WITH_IO` binding or a wrong `FOR` key type it points at the
specific token.

## Selective environment inheritance

Scripts no longer inherit the caller's environment wholesale. Except for the host-mirrored essentials documented in [ENV_CONTRACTS.md](https://github.com/jzombie/rust-oxdock/blob/main/ENV_CONTRACTS.md), host variables stay private unless you opt in explicitly.

- Add `INHERIT_ENV [FOO, BAR, BAZ]` at the very top of the script to copy those keys from the process environment before any other command runs.
- The directive must be top-level: no guards, no surrounding blocks, and no repeats. Trying to nest or guard it triggers a parser error so scripts stay deterministic.
- Subsequent `ENV` commands can override inherited values, similar to how Docker's `ENV` overrides `--env` flags.
- Test harnesses and embedders can supply values programmatically; the [environment-guards example](#environment-guards) injects `DEPLOY_TARGET` through the docs-conformance runner rather than the real process environment.

Keeping inheritance selective avoids leaking secrets by default while still allowing ergonomics for well-known keys (proxy settings, artifact caches, etc.).

Every variable the seed provides to spawned children is specified in [ENV_CONTRACTS.md](https://github.com/jzombie/rust-oxdock/blob/main/ENV_CONTRACTS.md).

## Path Separators

- **Cross-platform behavior:** Paths in OxDock scripts are treated as filesystem paths and are resolved using Rust's `Path`/`PathBuf` APIs. That means you can use either `/`-separated paths or `./`-prefixed relative paths in scripts and they will be interpreted correctly on Windows, macOS, and Linux.

- **Path separator preference / requirement:** For consistency and portability, OxDock scripts should use the forward slash (`/`) as the path separator in script source. While the runtime resolves paths using platform APIs and will accept platform-specific absolute paths, using `/` in scripts (even on Windows) avoids needing to escape backslashes (`\`) and matches Docker-style examples. If you must reference a native Windows absolute path, prefer the `C:/path/to` form or escape backslashes carefully.

- **Relative paths:** A leading `./` indicates a path relative to the current DSL working directory (the same semantics used by Docker). For example: `COPY ./src ./out` or `SYMLINK ./dir ./dir-link` will work on all platforms.

- **Absolute paths:** Use platform-appropriate absolute paths (e.g., `/usr/bin` on Unix-like systems, `C:\path\to` on Windows). OxDock will use the host OS path semantics when resolving absolute paths.

- **Symlinks and Windows:** Creating symlinks on Windows may require elevated permissions on some older OS versions; without permission the `SYMLINK` step fails with an error instead of falling back.

- **Globbing & shell expansion:** OxDock does not implicitly perform shell globbing or shell-side expansion for file arguments. When you need shell semantics use `RUN` with the platform shell, or add explicit DSL commands that accept wildcards if you want portable behavior.

## Glossary

- **AST**: The parsed tree of a script. Statements, declarations, and IO walk this tree one step at a time.
- **Descriptor**: One type's static singleton: its name, docs, and the vtable every value of that type carries.
- **Fat pointer**: A pointer that carries metadata alongside the address. Words avoid them: every payload type is sized, so payload pointers stay thin.
- **Heap**: Memory for data that outgrows the stack. Container contents and non-inline payloads live here, each owned by exactly one word.
- **Inline**: Payload bytes carried inside the word itself, with zero allocation. Available to `Copy` scalars that fit in 64 bits.
- **NaN boxing**: A technique that packs values into 64 bits by reusing NaN float patterns. Denser than 16 byte words at the cost of pointer masking and constrained host values.
- **Payload**: The 64 bit data half of a word: either inline bytes or a pointer to one owned box.
- **Plugin**: A host extension module ([NET](https://github.com/jzombie/rust-oxdock/blob/main/crates/plugins/oxdock-net-plugin/README.md), [SSH](https://github.com/jzombie/rust-oxdock/blob/main/crates/plugins/oxdock-ssh-plugin/README.md)) loaded with IMPORT, bringing extra functions into script scope.
- **Provenance**: The recorded origin of a pointer, which Rust uses to judge whether a memory access is valid. Round tripping through the same box type preserves it.
- **RPN**: Reverse Polish Notation: arithmetic compiled to a flat stack program instead of tree walking.
- **Vtable**: The operations half of a descriptor: function pointers that clone, drop, compare, and render values of that type.
- **Word**: The fixed 128 bit unit of every script value: a descriptor pointer plus a payload.
