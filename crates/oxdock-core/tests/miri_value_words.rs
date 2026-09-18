//! Miri validation for the value-word lifecycle (issue #146 follow-up).
//!
//! These tests exercise ONLY allocation, cloning, equality, formatting, and
//! dropping through descriptor vtables: no filesystem, no processes, no
//! threads, no environment. They run green under both `cargo test` and
//! `cargo +nightly miri test -p oxdock-core --test miri_value_words`.
//! A use-after-free, double-free, or provenance violation in the payload
//! casts fails loudly under Miri instead of corrupting silently.

use oxdock_func_macro::oxdock_type;
use oxdock_parser::{OxDockType, Value, startup_descriptors};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

#[test]
fn startup_descriptors_cover_ten_types() {
    // The startup directory is fixed and self-describing: names in order,
    // each entry its payload type's own singleton.
    let descriptors = startup_descriptors();
    let names: Vec<&str> = descriptors.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "INT", "FLOAT", "STRING", "BOOL", "LIST", "MAP", "PATH", "DURATION", "PIPE", "HANDLE",
        ]
    );
    for (name, descriptor) in &descriptors {
        assert_eq!(descriptor.name, *name);
    }
    assert_eq!(Value::int(1).type_name(), "INT");
    assert_eq!(Value::string("s".to_string()).type_name(), "STRING");
}

#[test]
fn scalar_words_clone_drop_eq_fmt() {
    let values = vec![
        Value::int(0),
        Value::int(i64::MIN),
        Value::int(i64::MAX),
        Value::float(0.5),
        Value::float(-3.25),
        Value::bool(true),
        Value::bool(false),
        Value::handle(7),
    ];
    for value in &values {
        let clone = value.clone();
        assert_eq!(&clone, value);
        assert_eq!(format!("{clone}"), format!("{value}"));
        drop(clone);
        // Original stays valid after the clone drops (no shared ownership).
        assert_eq!(format!("{value}"), format!("{value}"));
    }
    assert_ne!(Value::int(1), Value::int(2));
    assert_ne!(Value::int(1), Value::float(1.0));
    assert_eq!(format!("{}", Value::int(42)), "42");
    assert_eq!(format!("{}", Value::bool(true)), "true");
    assert_eq!(format!("{}", Value::handle(9)), "task#9");
}

#[test]
fn heap_words_clone_independently_then_drop() {
    let original = Value::string("hello".to_string());
    let clone = original.clone();
    assert_eq!(&clone, &original);
    drop(original);
    // The clone owns a separate box: usable after the original drops.
    assert_eq!(format!("{clone}"), "\"hello\"");
    drop(clone);

    let path = {
        #[allow(clippy::disallowed_types)]
        let buf = std::path::PathBuf::from("a/b");
        Value::path(buf)
    };
    let path_clone = path.clone();
    drop(path);
    assert_eq!(format!("{path_clone}"), "a/b");

    let duration = Value::duration(Duration::from_secs(90));
    assert_eq!(format!("{duration}"), "90s");
    drop(duration);

    let pipe = Value::pipe_fresh();
    assert_eq!(format!("{pipe}"), "<pipe>");
    // Handles share the backend cell: clones stay usable after the
    // original drops, and equality is handle identity.
    let pipe_clone = pipe.clone();
    assert_eq!(&pipe_clone, &pipe);
    drop(pipe);
    assert_eq!(format!("{pipe_clone}"), "<pipe>");
    let other = Value::pipe_fresh();
    // Distinct declarations never alias: identity, not byte comparison.
    assert_ne!(&other, &pipe_clone);
    drop(other);
    drop(pipe_clone);
}

#[test]
fn shared_containers_share_buffers_then_drop() {
    // `LIST` and `MAP` are shared heaps (issue #152): cloning bumps the
    // strong count in O(1) with no allocation, so clones point at the same
    // buffer and stay usable after the original drops. Exclusive heaps
    // (`STRING`) still deep-copy: pointers differ.
    let text = Value::string("hello".to_string());
    let text_clone = text.clone();
    assert_ne!(text.heap_ptr(), text_clone.heap_ptr());
    drop(text);
    drop(text_clone);

    let list = Value::list(vec![Value::int(1), Value::string("x".to_string())]);
    let list_clone = list.clone();
    assert_eq!(list.heap_ptr(), list_clone.heap_ptr());
    assert_eq!(&list_clone, &list);
    drop(list);
    assert_eq!(format!("{list_clone}"), "[1, \"x\"]");
    drop(list_clone);

    let mut entries = BTreeMap::new();
    entries.insert("k".to_string(), Value::int(1));
    let map = Value::map(entries);
    let map_clone = map.clone();
    assert_eq!(map.heap_ptr(), map_clone.heap_ptr());
    drop(map);
    assert_eq!(format!("{map_clone}"), "{k: 1}");
    drop(map_clone);
}

#[test]
fn shared_mutation_detaches_clones() {
    // Copy-on-write through the only mutable choke point
    // (`as_list_mut` / `as_map_mut`): mutating a shared word clones the
    // buffer first, so siblings never observe the write.
    let mut original = Value::list(vec![Value::int(1)]);
    let clone = original.clone();
    assert_eq!(original.heap_ptr(), clone.heap_ptr());
    original.as_list_mut().expect("list").push(Value::int(2));
    assert_eq!(format!("{original}"), "[1, 2]");
    assert_eq!(format!("{clone}"), "[1]");
    assert_ne!(original.heap_ptr(), clone.heap_ptr());
    drop(original);
    drop(clone);

    let mut entries = BTreeMap::new();
    entries.insert("k".to_string(), Value::int(1));
    let mut original = Value::map(entries);
    let clone = original.clone();
    original
        .as_map_mut()
        .expect("map")
        .insert("j".to_string(), Value::int(2));
    assert_eq!(format!("{original}"), "{j: 2, k: 1}");
    assert_eq!(format!("{clone}"), "{k: 1}");
    drop(original);
    drop(clone);
}

#[test]
fn unique_mutation_stays_in_place() {
    // Strong count 1: `unshare` hands out the live buffer with no copy.
    let mut solo = Value::list(vec![Value::int(1)]);
    let before = solo.heap_ptr();
    solo.as_list_mut().expect("list").push(Value::int(2));
    assert_eq!(solo.heap_ptr(), before);
    assert_eq!(format!("{solo}"), "[1, 2]");

    let mut widget = make_widget("gear");
    let before = widget.heap_ptr();
    widget
        .read_heap_mut::<Widget>(Widget::descriptor())
        .expect("widget")
        .mass = 10;
    assert_eq!(widget.heap_ptr(), before);
    assert_eq!(format!("{widget}"), "widget(gear:10)");
}

#[test]
#[should_panic(expected = "no heap buffer")]
fn inline_read_heap_mut_panics() {
    let mut entity = Value::mint_inline(EntityId::descriptor(), EntityId(1));
    let _ = entity.read_heap_mut::<EntityId>(EntityId::descriptor());
}

#[test]
fn nested_heap_churn() {
    // Lists of maps of lists: recursive drop must free every buffer exactly
    // once. Container clones share the outer buffer (issue #152); dropping
    // the root first leaves every clone usable.
    let mut inner = BTreeMap::new();
    inner.insert("v".to_string(), Value::int(3));
    let root = Value::list(vec![Value::map(inner), Value::string("tail".to_string())]);
    let mut clones = Vec::new();
    for _ in 0..16 {
        clones.push(root.clone());
    }
    drop(root);
    for clone in &clones {
        assert_eq!(format!("{clone}"), "[{v: 3}, \"tail\"]");
    }
    clones.clear();
}

/// Opaque widget type.
///
/// The annotated struct IS the payload: the macro derives monomorphic heap
/// adapters over `Widget`, the same derivation the startup-registered types
/// use.
#[oxdock_type(name = "WIDGET")]
#[derive(Debug, Clone, PartialEq)]
struct Widget {
    label: String,
    mass: i64,
}

impl fmt::Display for Widget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "widget({}:{})", self.label, self.mass)
    }
}

fn make_widget(label: &str) -> Value {
    Value::mint_heap(
        Widget::descriptor(),
        Widget {
            label: label.to_string(),
            mass: 9,
        },
    )
}

#[test]
fn custom_words_clone_drop_eq_fmt() {
    // The descriptor is the payload type's own singleton: identical
    // pointers every call, no table, no lock.
    assert!(std::ptr::eq(Widget::descriptor(), Widget::descriptor()));
    assert_eq!(Widget::descriptor().name, "WIDGET");
    assert_eq!(Widget::descriptor().summary, "Opaque widget type.");

    let widget = make_widget("gear");
    assert_eq!(widget.type_name(), "WIDGET");
    assert_eq!(format!("{widget}"), "widget(gear:9)");

    // Clone survives the original's drop: the regression that once SIGBUSed
    // came from stripping the trait-object vtable here.
    let clone = widget.clone();
    assert_eq!(&clone, &widget);
    drop(widget);
    assert_eq!(format!("{clone}"), "widget(gear:9)");

    let other = make_widget("gear");
    assert_eq!(&clone, &other);
    let different = make_widget("spring");
    assert_ne!(&clone, &different);
    assert_ne!(&clone, &Value::string("widget(gear:9)".to_string()));
    drop(clone);
    drop(other);
    drop(different);
}

#[test]
fn words_share_one_representation() {
    // Dogfooding proof: scalars ride inline in the payload (zero
    // allocation) while heap values ride behind a thin pointer, and both
    // go through the same descriptor vtable. There is no privileged
    // representation left to detect.
    let int = Value::int(41);
    assert_eq!(int.inline_bits(), 41);
    assert_eq!(format!("{int}"), "41");

    let text = Value::string("hi".to_string());
    assert!(!text.heap_ptr().is_null());
    drop(text);
    drop(int);
}

#[test]
fn inline_host_scalars_ride_zero_alloc() {
    // A host `Copy` scalar annotated `inline` uses the exact same inline
    // path as the startup integer/boolean words. Minting needs no
    // registry: the descriptor singleton rides the payload type.
    let entity = Value::mint_inline(EntityId::descriptor(), EntityId(0xC0FFEE));
    assert_eq!(entity.type_name(), "ENTITY");
    assert_eq!(entity.inline_bits(), 0xC0FFEE);
    assert_eq!(format!("{entity}"), "entity#12648430");
    let clone = entity.clone();
    assert_eq!(&clone, &entity);
    drop(entity);
    drop(clone);
}

/// Host entity handle: inline payload, same derivation as startup scalars.
#[oxdock_type(name = "ENTITY", inline)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct EntityId(u64);

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "entity#{}", self.0)
    }
}

#[test]
fn startup_directory_lists_ten_types() {
    // No registry query: the startup directory is a fixed static list.
    let names: Vec<&str> = startup_descriptors()
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        names,
        [
            "INT", "FLOAT", "STRING", "BOOL", "LIST", "MAP", "PATH", "DURATION", "PIPE", "HANDLE",
        ]
    );
}
