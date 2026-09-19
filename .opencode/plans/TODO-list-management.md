If list manipulation is being added to `STD`, `LEN` is the single most urgent primitive to implement. Without it, scripts cannot inspect dynamically populated collections, bounds-check array reads, or drive standard `WHILE` iteration over accumulated handles.

### 1. `LEN` Implementation (`crates/plugins/oxdock-std-plugin`)

`LEN` should be a pure host function accepting any sequence or collection type (`STRING`, `LIST`, `MAP`):

```rust
#[oxdock_func(pure, returns = "INT", summary = "Return length of a LIST, STRING, or MAP.")]
fn len(value: Value) -> Result<Value> {
    match value {
        Value::List(l) => Ok(Value::Int(l.len() as i64)),
        Value::String(s) => Ok(Value::Int(s.chars().count() as i64)),
        Value::Map(m) => Ok(Value::Int(m.len() as i64)),
        other => bail!("LEN expects LIST, STRING, or MAP, got {}", other.type_name()),
    }
}

```

### 2. `SLICE` Implementation

`SLICE` provides functional element removal without requiring tuple destructuring or mutable `POP` semantics:

```rust
#[oxdock_func(pure, returns = "LIST", summary = "Slice a LIST from start (inclusive) to end (exclusive).")]
fn slice(list: Value, start: i64, end: i64) -> Result<Value> {
    let Value::List(items) = list else {
        bail!("SLICE expects a LIST, got {}", list.type_name());
    };

    let len = items.len() as i64;
    let s = start.clamp(0, len) as usize;
    let e = end.clamp(s as i64, len) as usize;

    Ok(Value::List(items[s..e].to_vec()))
}

```

### DSL Usage

```oxdock
LET $items: LIST = [10, 20, 30, 40]

# Inspect size
LET $count: INT = LEN($items)  # 4

# Trim trailing element (functional POP)
$items = SLICE($items, 0, LEN($items) - 1)  # [10, 20, 30]

```

Adding `PUSH`, `LEN`, and `SLICE` to `STD` gives complete write, inspection, and truncation support for lists while keeping value semantics intact.
