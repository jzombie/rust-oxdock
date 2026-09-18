//! Value-word core: every DSL value is a fixed-size word (a [`TypeDescriptor`]
//! vtable pointer plus a 64-bit [`ValuePayload`]) interpreted through that
//! vtable.
//!
//! There is exactly one representation for every type. Payloads that fit in
//! 64 bits (integers, floats, booleans, handles, and host scalars annotated
//! `#[oxdock_type(inline)]`) ride directly in the payload; everything else
//! rides behind a thin pointer to either an owned `Box<T>` (exclusive heaps:
//! `STRING`, `PATH`, `DURATION`, `PIPE`, most host types) or a shared
//! `Arc<T>` (shared heaps: `LIST`, `MAP`, and host types annotated
//! `#[oxdock_type(shared)]`). The vtable owns the lifecycle (`clone`, `drop`)
//! and operations (`eq`, `fmt`), so `Clone`/`Drop`/`PartialEq`/`Display` on
//! [`Value`] delegate instead of matching. There are no dynamic trait
//! objects anywhere in this path: every hook is a monomorphic function
//! pointer reached directly, with no table lookup and no lock.
//!
//! Descriptors are canonical singletons: each `#[oxdock_type]` struct gets
//! one `&'static TypeDescriptor` (built at compile time, shared by every
//! word of that type), so words carry their own vtable and no registry of
//! any kind exists. The ten startup types (`INT`, `FLOAT`, `STRING`, `BOOL`,
//! `LIST`, `MAP`, `PATH`, `DURATION`, `PIPE`, `HANDLE`) are ordinary Rust
//! structs annotated with `#[oxdock_type]`, exactly as host types are. Name
//! directories (which descriptor answers for `"TAG"`) live per execution
//! state in `oxdock-core`, never here: this module knows types, not names.
//!
//! Ownership discipline (load-bearing, Miri-verified in
//! `crates/oxdock-core/tests/miri_value_words.rs`):
//!
//! - Exclusive heap [`Value`]s own their box exactly once. `clone` allocates
//!   a new box; `drop` frees it. No sharing, no aliasing. Because each box
//!   holds a concrete sized `T`, its pointer is thin: no double-boxing, no
//!   fat pointer casts, no metadata to lose.
//! - Shared heap [`Value`]s (`LIST`, `MAP`) co-own an `Arc<T>` buffer.
//!   `clone` bumps the strong count in `O(1)` with no allocation; `drop`
//!   releases one count and frees only the final word's drop. Because the
//!   DSL exposes no interior mutability, aliases, or reference syntax,
//!   container graphs are strictly acyclic trees, so refcounting reclaims
//!   deterministically with no tracing collector. Mutable access goes only
//!   through [`Value::read_heap_mut`], which detaches (clones the buffer)
//!   whenever the strong count exceeds 1, so a writer always exclusively
//!   owns a private buffer and clones never observe each other's writes.
//!   Deriving `&mut` from a payload any other way is unsound.
//! - Pointer casts are always `Box::into_raw` / `Box::from_raw` (exclusive)
//!   or `Arc::into_raw` / `Arc::from_raw` plus `Arc::increment_strong_count`
//!   (shared) round trips on the same concrete payload type, which preserves
//!   provenance. Inline words never touch the pointer domain; heap words
//!   never touch the integer domain.
//! - Minting a word with a descriptor built for a different Rust type
//!   misdirects the vtable and is unsound. The `mint_*` constructors
//!   document this contract; hosts mint through the payload type's own
//!   `OxDockType::descriptor()`, which cannot mismatch by construction.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use oxdock_func_macro::oxdock_type;
use oxdock_pipe::{PipeHandle, new_handle};

/// Anchor of a type's reference section, derived from its name the way the
/// Markdown slugger derives it from the doc title.
pub fn type_anchor(name: &str) -> String {
    format!("value-type-{}", name.to_lowercase())
}

/// Canonical descriptors of the ten startup types, in a fixed order, for
/// seeding per-state name directories and static rendering (docs-gen).
/// Each entry is the payload struct's own singleton: no table, no lock.
pub fn startup_descriptors() -> [(&'static str, &'static TypeDescriptor); 10] {
    [
        ("INT", IntValue::descriptor()),
        ("FLOAT", FloatValue::descriptor()),
        ("STRING", StringValue::descriptor()),
        ("BOOL", BoolValue::descriptor()),
        ("LIST", ListValue::descriptor()),
        ("MAP", MapValue::descriptor()),
        ("PATH", PathValue::descriptor()),
        ("DURATION", DurationValue::descriptor()),
        ("PIPE", PipeValue::descriptor()),
        ("HANDLE", HandleValue::descriptor()),
    ]
}

// ---------------------------------------------------------------------------
// Payload structs for the startup-registered types. Each carries
// `#[oxdock_type]` so its descriptor derives from the same macro hosts use;
// `inline` selects the zero-allocation payload path, exactly as for host
// scalars. Private: hosts never name these types; they observe them through
// the word accessors below.
// ---------------------------------------------------------------------------

/// 64-bit signed integer, e.g. an exit code.
#[oxdock_type(crate_path = "::oxdock_parser", name = "INT", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct IntValue(pub i64);

/// 64-bit float, e.g. a ratio.
#[oxdock_type(crate_path = "::oxdock_parser", name = "FLOAT", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatValue(pub f64);

/// Arbitrary text. Quotes keep exact bytes, lone `$var` evaluates, `{{ ... }}` interpolates.
#[oxdock_type(
    crate_path = "::oxdock_parser",
    name = "STRING",
    summary = "Arbitrary text."
)]
#[derive(Debug, Clone, PartialEq)]
struct StringValue(pub String);

/// Boolean `true` or `false`.
#[oxdock_type(crate_path = "::oxdock_parser", name = "BOOL", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct BoolValue(pub bool);

/// Ordered list of values. Shared heap: cloning bumps a refcount.
#[oxdock_type(crate_path = "::oxdock_parser", name = "LIST", shared)]
#[derive(Debug, Clone, PartialEq)]
struct ListValue(pub Vec<Value>);

/// String-keyed map of values. Shared heap: cloning bumps a refcount.
#[oxdock_type(crate_path = "::oxdock_parser", name = "MAP", shared)]
#[derive(Debug, Clone, PartialEq)]
struct MapValue(pub BTreeMap<String, Value>);

/// Workspace path, resolved against cwd and guarded against escape.
#[oxdock_type(crate_path = "::oxdock_parser", name = "PATH")]
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::disallowed_types)]
struct PathValue(#[allow(clippy::disallowed_types)] pub std::path::PathBuf);

/// Positive time span: `500ms`, `10s`, `2m`, `1h`; bare number means seconds.
#[oxdock_type(
    crate_path = "::oxdock_parser",
    name = "DURATION",
    summary = "Positive time span."
)]
#[derive(Debug, Clone, PartialEq)]
struct DurationValue(pub Duration);

/// Anonymous pipe handle. The backend materializes lazily on first
/// binding (never eagerly at declaration), so the choice always has full
/// usage context. Cloning shares the backend (explicit-sharing fan-out);
/// equality is handle identity, never byte comparison.
#[oxdock_type(
    crate_path = "::oxdock_parser",
    name = "PIPE",
    summary = "Anonymous pipe handle.",
    shared
)]
#[derive(Debug, Clone)]
struct PipeValue(pub PipeHandle);

impl PartialEq for PipeValue {
    /// Handle identity: two words name the same channel iff they share
    /// the cell. Never compares bytes (backends may be unbound, and
    /// locking two cells in `eq` risks ordering deadlocks).
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Background ASYNC task handle for AWAIT/CANCEL.
#[oxdock_type(crate_path = "::oxdock_parser", name = "HANDLE", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct HandleValue(pub u64);

impl fmt::Display for IntValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for FloatValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for BoolValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for HandleValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "task#{}", self.0)
    }
}

impl fmt::Display for StringValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "\"{}\"", self.0)
    }
}

impl fmt::Display for ListValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[")?;
        for (i, item) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", item)?;
        }
        write!(f, "]")
    }
}

impl fmt::Display for MapValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{")?;
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}: {}", k, v)?;
        }
        write!(f, "}}")
    }
}

impl fmt::Display for DurationValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", crate::command::format_duration(&self.0))
    }
}

impl fmt::Display for PathValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl fmt::Display for PipeValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<pipe>")
    }
}

/// Payload half of a [`Value`] word: either the value's bytes inline or a
/// thin pointer to an owned `Box<T>` (exclusive heaps) or a shared `Arc<T>`
/// (shared heaps), as the type's descriptor dictates.
/// Inline and pointer domains never mix for a given [`TypeDescriptor`].
///
/// Fields are private so safe code cannot forge payloads: every payload
/// enters a word through [`store_inline`] (inline bytes) or the
/// [`Value::mint_heap`] / [`Value::mint_heap_shared`] / [`Value::mint_inline`]
/// choke points, where the `Send + Sync + 'static` bounds are enforced.
/// External code observes payload bits through [`Value::inline_bits`] and
/// [`Value::heap_ptr`].
#[repr(C)]
#[derive(Clone, Copy)]
pub union ValuePayload {
    as_u64: u64,
    as_ptr: *mut (),
}

// Raw pointers are not `Send`/`Sync`, so both are implemented by hand.
// Soundness: fields are private, so every exclusive heap [`Value`] owns its
// box exactly once (mint allocates, `clone` allocates, `drop` frees),
// shared heap [`Value`]s co-own their `Arc` buffer (mint allocates with
// count 1, `clone` bumps, `drop` releases), payloads are never mutably
// aliased, no vtable hook writes through a shared reference, and heap
// contents are `Send + Sync` by construction (enforced at the `mint_*`
// choke points, the only construction path; `Arc<T>` itself is `Send + Sync`
// exactly when `T` is, which the same bounds guarantee).
unsafe impl Send for ValuePayload {}
unsafe impl Sync for ValuePayload {}

/// A DSL value: a [`TypeDescriptor`] vtable pointer plus a [`ValuePayload`].
/// Fixed size (128 bits on 64-bit targets). Lifecycle and operations call
/// the vtable directly, with no table lookup and no lock; see the module
/// docs for the ownership discipline.
//
// Fields are private so safe code cannot forge words with dangling
// pointers: construction flows through [`Value::mint_inline`],
// [`Value::mint_heap`], or the typed constructors below, and typed reads
// go through [`Value::read_inline`] / [`Value::read_heap`].
// `Send`/`Sync` follow from the payload impls above plus shared references.
unsafe impl Send for Value {}
unsafe impl Sync for Value {}
#[repr(C)]
pub struct Value {
    vtable: &'static TypeDescriptor,
    payload: ValuePayload,
}

impl Value {
    /// The word's canonical descriptor singleton: the vtable backing its
    /// lifecycle and operations.
    pub fn descriptor(&self) -> &'static TypeDescriptor {
        self.vtable
    }

    /// The word's registered type name (the descriptor's name).
    pub fn type_name(&self) -> &'static str {
        self.vtable.name
    }

    /// Raw payload bits, copied out. Meaningful for inline words (the
    /// value's bytes); for heap words these are the box pointer's bits.
    pub fn inline_bits(&self) -> u64 {
        unsafe { self.payload.as_u64 }
    }

    /// Heap box (exclusive) or buffer (shared) pointer, copied out. Only
    /// meaningful for heap words; never dereferenced here. Reading (not
    /// dereferencing) is safe.
    pub fn heap_ptr(&self) -> *mut () {
        unsafe { self.payload.as_ptr }
    }

    /// Mint an inline word: memcpy the value's bytes into the payload.
    /// Zero allocation. The descriptor must be the payload type's own
    /// `OxDockType::descriptor()`; mismatching them misdirects the vtable
    /// and is unsound.
    pub fn mint_inline<T>(descriptor: &'static TypeDescriptor, value: T) -> Self
    where
        T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        Self {
            vtable: descriptor,
            payload: store_inline(value),
        }
    }

    /// Mint an exclusive heap word: move the value into an owned `Box<T>`
    /// behind a thin pointer. One box allocation. The descriptor must be the
    /// payload type's own `OxDockType::descriptor()`; mismatching them
    /// misdirects the vtable and is unsound.
    pub fn mint_heap<T>(descriptor: &'static TypeDescriptor, value: T) -> Self
    where
        T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        Self {
            vtable: descriptor,
            payload: ValuePayload {
                as_ptr: Box::into_raw(Box::new(value)) as *mut (),
            },
        }
    }

    /// Mint a shared heap word: move the value into a reference-counted
    /// `Arc<T>` behind a thin pointer. One allocation; later clones bump the
    /// strong count instead of copying. The descriptor must be the payload
    /// type's own `OxDockType::descriptor()` built for the shared path
    /// (`#[oxdock_type(shared)]`); mismatching them misdirects the vtable
    /// and is unsound.
    pub fn mint_heap_shared<T>(descriptor: &'static TypeDescriptor, value: T) -> Self
    where
        T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        Self {
            vtable: descriptor,
            payload: ValuePayload {
                as_ptr: std::sync::Arc::into_raw(std::sync::Arc::new(value)) as *mut (),
            },
        }
    }

    /// Read an inline word back out. Returns `None` when the word carries
    /// a different descriptor; the load itself is infallible for a word
    /// minted for `T`.
    pub fn read_inline<T>(&self, expected: &'static TypeDescriptor) -> Option<T>
    where
        T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        if !std::ptr::eq(self.vtable, expected) {
            return None;
        }
        Some(unsafe { load_inline::<T>(self.payload) })
    }

    /// Borrow a heap word's concrete value. Returns `None` when the word
    /// carries a different descriptor.
    pub fn read_heap<T>(&self, expected: &'static TypeDescriptor) -> Option<&T>
    where
        T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        if !std::ptr::eq(self.vtable, expected) {
            return None;
        }
        Some(unsafe { &*(self.payload.as_ptr as *const T) })
    }

    /// Borrow a heap word's concrete value mutably, detaching shared buffers
    /// first (copy-on-write). Returns `None` when the word carries a
    /// different descriptor. This is the only sound way to obtain `&mut`
    /// access to a heap payload: exclusive heaps hand out their box
    /// directly, shared heaps clone-then-hand-out when the strong count
    /// exceeds 1 and mutate in place otherwise. Panics when called with an
    /// inline descriptor, which has no heap buffer.
    pub fn read_heap_mut<T>(&mut self, expected: &'static TypeDescriptor) -> Option<&mut T>
    where
        T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        if !std::ptr::eq(self.vtable, expected) {
            return None;
        }
        Some(unsafe { &mut *((self.vtable.unshare)(&mut self.payload) as *mut T) })
    }

    /// Construct an integer word (inline, zero allocation).
    pub fn int(n: i64) -> Self {
        Self::mint_inline(IntValue::descriptor(), IntValue(n))
    }

    /// Construct a float word (inline, zero allocation).
    pub fn float(f: f64) -> Self {
        Self::mint_inline(FloatValue::descriptor(), FloatValue(f))
    }

    /// Construct a boolean word (inline, zero allocation).
    pub fn bool(b: bool) -> Self {
        Self::mint_inline(BoolValue::descriptor(), BoolValue(b))
    }

    /// Construct a task-handle word (inline, zero allocation).
    pub fn handle(id: u64) -> Self {
        Self::mint_inline(HandleValue::descriptor(), HandleValue(id))
    }

    /// Construct a string word.
    pub fn string(s: String) -> Self {
        Self::mint_heap(StringValue::descriptor(), StringValue(s))
    }

    /// Construct a list word (shared heap: clones share the buffer).
    pub fn list(items: Vec<Value>) -> Self {
        Self::mint_heap_shared(ListValue::descriptor(), ListValue(items))
    }

    /// Construct a map word (shared heap: clones share the buffer).
    pub fn map(entries: BTreeMap<String, Value>) -> Self {
        Self::mint_heap_shared(MapValue::descriptor(), MapValue(entries))
    }

    /// Construct a path word.
    #[allow(clippy::disallowed_types)]
    pub fn path(p: std::path::PathBuf) -> Self {
        Self::mint_heap(PathValue::descriptor(), PathValue(p))
    }

    /// Construct a duration word.
    pub fn duration(d: Duration) -> Self {
        Self::mint_heap(DurationValue::descriptor(), DurationValue(d))
    }

    /// Construct a fresh unbound pipe handle (`LET $p: PIPE`, host
    /// `new_pipe()`). Materializes lazily on first binding.
    pub fn pipe_fresh() -> Self {
        Self::mint_heap_shared(PipeValue::descriptor(), PipeValue(new_handle()))
    }

    /// Wrap an existing handle as a `PIPE` word. Clones share the backend.
    pub fn pipe_handle(handle: PipeHandle) -> Self {
        Self::mint_heap_shared(PipeValue::descriptor(), PipeValue(handle))
    }

    /// Read an integer payload. Returns `None` for non-`INT` words.
    pub fn as_i64(&self) -> Option<i64> {
        self.read_inline::<IntValue>(IntValue::descriptor())
            .map(|v| v.0)
    }

    /// Read a float payload. Returns `None` for non-`FLOAT` words.
    pub fn as_f64(&self) -> Option<f64> {
        self.read_inline::<FloatValue>(FloatValue::descriptor())
            .map(|v| v.0)
    }

    /// Read a boolean payload. Returns `None` for non-`BOOL` words.
    pub fn as_bool(&self) -> Option<bool> {
        self.read_inline::<BoolValue>(BoolValue::descriptor())
            .map(|v| v.0)
    }

    /// Read a task-handle payload. Returns `None` for non-`HANDLE` words.
    pub fn as_handle(&self) -> Option<u64> {
        self.read_inline::<HandleValue>(HandleValue::descriptor())
            .map(|v| v.0)
    }

    /// Borrow a string payload. Returns `None` for non-`STRING` words.
    pub fn as_str(&self) -> Option<&str> {
        self.read_heap::<StringValue>(StringValue::descriptor())
            .map(|v| v.0.as_str())
    }

    /// Borrow a list payload. Returns `None` for non-`LIST` words.
    pub fn as_list(&self) -> Option<&Vec<Value>> {
        self.read_heap::<ListValue>(ListValue::descriptor())
            .map(|v| &v.0)
    }

    /// Borrow a list payload mutably, detaching the shared buffer first when
    /// clones exist. Returns `None` for non-`LIST` words. This is the choke
    /// point every future in-place container mutation must go through.
    pub fn as_list_mut(&mut self) -> Option<&mut Vec<Value>> {
        self.read_heap_mut::<ListValue>(ListValue::descriptor())
            .map(|v| &mut v.0)
    }

    /// Borrow a map payload. Returns `None` for non-`MAP` words.
    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        self.read_heap::<MapValue>(MapValue::descriptor())
            .map(|v| &v.0)
    }

    /// Borrow a map payload mutably, detaching the shared buffer first when
    /// clones exist. Returns `None` for non-`MAP` words. This is the choke
    /// point every future in-place container mutation must go through.
    pub fn as_map_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        self.read_heap_mut::<MapValue>(MapValue::descriptor())
            .map(|v| &mut v.0)
    }

    /// Clone the pipe handle out of a `PIPE` word. Returns `None` for
    /// non-`PIPE` words. The clone shares the backend cell.
    pub fn as_pipe_handle(&self) -> Option<PipeHandle> {
        self.read_heap::<PipeValue>(PipeValue::descriptor())
            .map(|v| v.0.clone())
    }

    /// Read a duration payload. Returns `None` for non-`DURATION` words.
    pub fn as_duration(&self) -> Option<Duration> {
        self.read_heap::<DurationValue>(DurationValue::descriptor())
            .map(|v| v.0)
    }

    /// Borrow a path payload. Returns `None` for non-`PATH` words.
    #[allow(clippy::disallowed_types)]
    pub fn as_path(&self) -> Option<&std::path::Path> {
        self.read_heap::<PathValue>(PathValue::descriptor())
            .map(|v| v.0.as_path())
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        let payload = unsafe { (self.vtable.clone)(self.payload) };
        Self {
            vtable: self.vtable,
            payload,
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self.payload) };
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        if !std::ptr::eq(self.vtable, other.vtable) {
            return false;
        }
        unsafe { (self.vtable.eq)(self.payload, other.payload) }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}(", self.vtable.name)?;
        unsafe { (self.vtable.fmt)(self.payload, f) }?;
        write!(f, ")")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        unsafe { (self.vtable.fmt)(self.payload, f) }
    }
}

/// Export hook for a DSL payload type, implemented by `#[oxdock_type]` on
/// the payload struct itself. The canonical descriptor singleton backs
/// every word of the type; user code never names a generated symbol.
pub trait OxDockType {
    /// The canonical descriptor deriving from the struct's name plus doc
    /// comments. The same reference every call: pointer-compare words
    /// against it.
    fn descriptor() -> &'static TypeDescriptor;
}

/// Vtable for one type: lifecycle plus operations. All hooks are plain
/// function pointers (never closures) so descriptors stay `Copy` and the
/// global table hands them out by value. Every hook documents the payload
/// domain it expects; calling one with a foreign payload is unsound, and
/// every call site is a single choke point reviewed with the layout.
///
/// `unshare` is the copy-on-write gate: it rewrites the payload to a
/// uniquely owned buffer when necessary and returns a mutable pointer the
/// caller exclusively owns. Mutation must always go through
/// [`Value::read_heap_mut`]; deriving `&mut` from a payload any other way
/// is unsound for shared heaps.
#[derive(Clone, Copy)]
pub struct TypeDescriptor {
    pub name: &'static str,
    pub summary: &'static str,
    pub docs: &'static str,
    pub clone: unsafe fn(ValuePayload) -> ValuePayload,
    pub drop: unsafe fn(ValuePayload),
    pub eq: unsafe fn(ValuePayload, ValuePayload) -> bool,
    pub fmt: unsafe fn(ValuePayload, &mut fmt::Formatter<'_>) -> fmt::Result,
    pub unshare: unsafe fn(&mut ValuePayload) -> *mut (),
}

// ---------------------------------------------------------------------------
// Payload adapters: one inline set, one exclusive-heap set, and one shared-
// heap set drive `clone`/`drop`/`eq`/`fmt`/`unshare` for every type through
// monomorphic function pointers. These are `pub` solely so `#[oxdock_type]`-
// generated descriptors can name them; hosts never call them directly.
// ---------------------------------------------------------------------------

/// Copy a `Copy` value's bytes into a payload. Panics when `T` exceeds 64
/// bits: such types must use the heap path.
pub fn store_inline<T>(value: T) -> ValuePayload
where
    T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    assert!(
        std::mem::size_of::<T>() <= 8,
        "inline payloads hold at most 64 bits"
    );
    let mut bits: u64 = 0;
    unsafe {
        std::ptr::copy_nonoverlapping(
            &value as *const T as *const u8,
            &mut bits as *mut u64 as *mut u8,
            std::mem::size_of::<T>(),
        );
    }
    // No `mem::forget`: `T: Copy` has no finalizer, so the source needs no
    // suppression after its bytes are copied out.
    ValuePayload { as_u64: bits }
}

/// Reconstruct a `Copy` value from an inline payload.
///
/// # Safety
/// The payload must hold bytes stored by [`store_inline`] for `T`.
pub unsafe fn load_inline<T>(payload: ValuePayload) -> T
where
    T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    // (Spelled as an explicit gate because `debug_assert!` expands to the
    // banned `cfg!` macro.)
    #[cfg(debug_assertions)]
    if std::mem::size_of::<T>() > 8 {
        panic!("inline payloads hold at most 64 bits");
    }
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(
            &payload.as_u64 as *const u64 as *const u8,
            value.as_mut_ptr() as *mut u8,
            std::mem::size_of::<T>(),
        );
        value.assume_init()
    }
}

/// Inline `clone`: payloads are plain bytes.
///
/// # Safety
/// The payload must hold inline bytes (never a live pointer).
pub unsafe fn clone_copy(payload: ValuePayload) -> ValuePayload {
    payload
}

/// Inline `drop`: nothing owns anything.
///
/// # Safety
/// The payload must hold inline bytes (never a live pointer).
pub unsafe fn drop_noop(_payload: ValuePayload) {}

/// Inline `eq`: reconstruct both sides and compare.
///
/// # Safety
/// Both payloads must hold bytes stored by [`store_inline`] for `T`.
pub unsafe fn eq_inline<T>(a: ValuePayload, b: ValuePayload) -> bool
where
    T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    unsafe { load_inline::<T>(a) == load_inline::<T>(b) }
}

/// Inline `fmt`: reconstruct and render.
///
/// # Safety
/// The payload must hold bytes stored by [`store_inline`] for `T`.
pub unsafe fn fmt_inline<T>(payload: ValuePayload, f: &mut fmt::Formatter) -> fmt::Result
where
    T: Copy + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    write!(f, "{}", unsafe { load_inline::<T>(payload) })
}

/// Heap `clone`: deep-copy the box.
///
/// # Safety
/// The payload must own a `Box<T>` exactly once.
pub unsafe fn clone_boxed<T>(payload: ValuePayload) -> ValuePayload
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let source = unsafe { &*(payload.as_ptr as *const T) };
    ValuePayload {
        as_ptr: Box::into_raw(Box::new(source.clone())) as *mut (),
    }
}
/// Heap `drop`: free the box.
///
/// # Safety
/// The payload must own a `Box<T>` exactly once; it must never be used
/// again afterwards.
pub unsafe fn drop_boxed<T>(payload: ValuePayload)
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    drop(unsafe { Box::from_raw(payload.as_ptr as *mut T) });
}

/// Heap `eq`: compare the boxed values.
///
/// # Safety
/// Both payloads must own a `Box<T>` exactly once.
pub unsafe fn eq_boxed<T>(a: ValuePayload, b: ValuePayload) -> bool
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let left = unsafe { &*(a.as_ptr as *const T) };
    let right = unsafe { &*(b.as_ptr as *const T) };
    left == right
}

/// Heap `fmt`: render the boxed value.
///
/// # Safety
/// The payload must own a `Box<T>` exactly once.
pub unsafe fn fmt_boxed<T>(payload: ValuePayload, f: &mut fmt::Formatter) -> fmt::Result
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let value = unsafe { &*(payload.as_ptr as *const T) };
    write!(f, "{value}")
}

/// Shared-heap `clone`: bump the `Arc` strong count, sharing the buffer.
/// `O(1)` with no allocation.
///
/// # Safety
/// The payload must co-own an `Arc<T>` buffer minted by
/// [`Value::mint_heap_shared`] and cloned only through this hook, so one
/// outstanding strong count exists per live word.
pub unsafe fn clone_shared<T>(payload: ValuePayload) -> ValuePayload
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    unsafe { std::sync::Arc::increment_strong_count(payload.as_ptr as *const T) };
    payload
}

/// Shared-heap `drop`: release one `Arc` strong count, freeing the buffer
/// only when the final word drops.
///
/// # Safety
/// The payload must co-own an `Arc<T>` buffer; it must never be used again
/// afterwards.
pub unsafe fn drop_shared<T>(payload: ValuePayload)
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    drop(unsafe { std::sync::Arc::from_raw(payload.as_ptr as *const T) });
}

/// Shared-heap `eq`: compare the shared values.
///
/// # Safety
/// Both payloads must co-own an `Arc<T>` buffer.
pub unsafe fn eq_shared<T>(a: ValuePayload, b: ValuePayload) -> bool
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let left = unsafe { &*(a.as_ptr as *const T) };
    let right = unsafe { &*(b.as_ptr as *const T) };
    left == right
}

/// Shared-heap `fmt`: render the shared value.
///
/// # Safety
/// The payload must co-own an `Arc<T>` buffer.
pub unsafe fn fmt_shared<T>(payload: ValuePayload, f: &mut fmt::Formatter) -> fmt::Result
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let value = unsafe { &*(payload.as_ptr as *const T) };
    write!(f, "{value}")
}

/// Inline `unshare`: inline words hold bytes, not a heap buffer, so there
/// is nothing to hand out mutably. Panics: reaching this hook means
/// [`Value::read_heap_mut`] was called with an inline descriptor, a caller
/// bug (mirrors [`store_inline`]'s size assert).
///
/// # Safety
/// The payload must hold inline bytes (never a live pointer).
pub unsafe fn unshare_inline(payload: &mut ValuePayload) -> *mut () {
    let _ = payload;
    panic!("inline words have no heap buffer to unshare");
}

/// Exclusive-heap `unshare`: the box is already uniquely owned, so the
/// payload is returned unchanged with no allocation.
///
/// # Safety
/// The payload must own a `Box<T>` exactly once. The returned pointer must
/// only be written through while this word stays the sole owner.
pub unsafe fn unshare_boxed<T>(payload: &mut ValuePayload) -> *mut ()
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    unsafe { payload.as_ptr }
}

/// Shared-heap `unshare`: detach on write. When the strong count is 1 the
/// payload is returned unchanged (in-place, no allocation); otherwise the
/// buffer is cloned, the word is rewritten to the private buffer, and the
/// other clones keep the original. Either way the returned pointer addresses
/// a buffer this word uniquely owns.
///
/// # Safety
/// The payload must co-own an `Arc<T>` buffer minted by
/// [`Value::mint_heap_shared`] with one outstanding strong count per live
/// word. The returned pointer must only be written through while this word
/// stays the sole owner of its (possibly fresh) buffer.
pub unsafe fn unshare_shared<T>(payload: &mut ValuePayload) -> *mut ()
where
    T: Clone + PartialEq + fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    let raw = unsafe { payload.as_ptr } as *const T;
    let mut shared = unsafe { std::sync::Arc::from_raw(raw) };
    let unique = std::sync::Arc::make_mut(&mut shared);
    let out = unique as *mut T;
    payload.as_ptr = std::sync::Arc::into_raw(shared) as *mut ();
    out as *mut ()
}
