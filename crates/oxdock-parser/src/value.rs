//! Value-word core: every DSL value is a fixed-size word (a [`TypeDescriptor`]
//! vtable pointer plus a 64-bit [`ValuePayload`]) interpreted through that
//! vtable.
//!
//! There is exactly one representation for every type. Payloads that fit in
//! 64 bits (integers, floats, booleans, handles, and host scalars annotated
//! `#[oxdock_type(inline)]`) ride directly in the payload; everything else
//! rides behind a thin pointer to an owned `Box<T>` holding the concrete
//! Rust value. The vtable owns the lifecycle (`clone`, `drop`) and
//! operations (`eq`, `fmt`), so `Clone`/`Drop`/`PartialEq`/`Display` on
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
//! - Every heap [`Value`] owns its box exactly once. `clone` allocates a new
//!   box; `drop` frees it. No sharing, no aliasing. Because each box holds
//!   a concrete sized `T`, its pointer is thin: no double-boxing, no fat
//!   pointer casts, no metadata to lose.
//! - Pointer casts are always `Box::into_raw` / `Box::from_raw` round trips
//!   on the same concrete box type, which preserves provenance. Inline
//!   words never touch the pointer domain; heap words never touch the
//!   integer domain.
//! - Minting a word with a descriptor built for a different Rust type
//!   misdirects the vtable and is unsound. The `mint_*` constructors
//!   document this contract; hosts mint through the payload type's own
//!   `OxDockType::descriptor()`, which cannot mismatch by construction.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use oxdock_func_macro::oxdock_type;

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

/// Ordered list of values.
#[oxdock_type(crate_path = "::oxdock_parser", name = "LIST")]
#[derive(Debug, Clone, PartialEq)]
struct ListValue(pub Vec<Value>);

/// String-keyed map of values.
#[oxdock_type(crate_path = "::oxdock_parser", name = "MAP")]
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

/// Named script pipe. Validity is checked against the pipe registry at coercion time.
#[oxdock_type(
    crate_path = "::oxdock_parser",
    name = "PIPE",
    summary = "Named script pipe."
)]
#[derive(Debug, Clone, PartialEq)]
struct PipeValue(pub String);

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
        write!(f, "pipe:{}", self.0)
    }
}

/// Payload half of a [`Value`] word: either the value's bytes inline or a
/// thin pointer to an owned `Box<T>`, as the type's descriptor dictates.
/// Inline and pointer domains never mix for a given [`TypeDescriptor`].
///
/// Fields are private so safe code cannot forge payloads: every payload
/// enters a word through [`store_inline`] (inline bytes) or the
/// [`Value::mint_heap`] / [`Value::mint_inline`] choke points, where the
/// `Send + Sync + 'static` bounds are enforced. External code observes
/// payload bits through [`Value::inline_bits`] and [`Value::heap_ptr`].
#[repr(C)]
#[derive(Clone, Copy)]
pub union ValuePayload {
    as_u64: u64,
    as_ptr: *mut (),
}

// Raw pointers are not `Send`/`Sync`, so both are implemented by hand.
// Soundness: fields are private, so every heap [`Value`] owns its box
// exactly once (mint allocates, `clone` allocates, `drop` frees),
// payloads are never aliased, no vtable hook writes through a shared
// reference, and heap contents are `Send + Sync` by construction
// (enforced at the `mint_*` choke points, the only construction path).
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

    /// Heap box pointer, copied out. Only meaningful for heap words;
    /// never dereferenced here. Reading (not dereferencing) is safe.
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

    /// Mint a heap word: move the value into an owned `Box<T>` behind a
    /// thin pointer. One box allocation. The descriptor must be the payload
    /// type's own `OxDockType::descriptor()`; mismatching them misdirects
    /// the vtable and is unsound.
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

    /// Construct a list word.
    pub fn list(items: Vec<Value>) -> Self {
        Self::mint_heap(ListValue::descriptor(), ListValue(items))
    }

    /// Construct a map word.
    pub fn map(entries: BTreeMap<String, Value>) -> Self {
        Self::mint_heap(MapValue::descriptor(), MapValue(entries))
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

    /// Construct a pipe-name word.
    pub fn pipe(name: String) -> Self {
        Self::mint_heap(PipeValue::descriptor(), PipeValue(name))
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

    /// Borrow a map payload. Returns `None` for non-`MAP` words.
    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        self.read_heap::<MapValue>(MapValue::descriptor())
            .map(|v| &v.0)
    }

    /// Borrow a pipe-name payload. Returns `None` for non-`PIPE` words.
    pub fn as_pipe_name(&self) -> Option<&str> {
        self.read_heap::<PipeValue>(PipeValue::descriptor())
            .map(|v| v.0.as_str())
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
#[derive(Clone, Copy)]
pub struct TypeDescriptor {
    pub name: &'static str,
    pub summary: &'static str,
    pub docs: &'static str,
    pub clone: unsafe fn(ValuePayload) -> ValuePayload,
    pub drop: unsafe fn(ValuePayload),
    pub eq: unsafe fn(ValuePayload, ValuePayload) -> bool,
    pub fmt: unsafe fn(ValuePayload, &mut fmt::Formatter<'_>) -> fmt::Result,
}

// ---------------------------------------------------------------------------
// Payload adapters: one inline set and one heap set drive `clone`/`drop`/
// `eq`/`fmt` for every type through monomorphic function pointers. These
// are `pub` solely so `#[oxdock_type]`-generated descriptors can name them;
// hosts never call them directly.
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
