//! Type-system integration for the executor.
//!
//! Name directories (which descriptor answers for `"TAG"`) live per
//! execution state: one `HashMap` seeded with the startup descriptors,
//! extended by the run's host descriptors. Words carry their own vtable,
//! so lifecycle never consults any table; only name resolution (declaration
//! checking, `TYPES()`, `TYPE_DESCRIBE`) reads the state map.

pub use oxdock_parser::{
    OxDockType, TypeDescriptor, Value, ValuePayload, clone_boxed, clone_copy, clone_shared,
    drop_boxed, drop_noop, drop_shared, eq_boxed, eq_inline, eq_shared, fmt_boxed, fmt_inline,
    fmt_shared, load_inline, startup_descriptors, store_inline, type_anchor, unshare_boxed,
    unshare_inline, unshare_shared,
};

use std::collections::HashMap;

use oxdock_process::ProcessManager;

use super::state::ExecState;

/// Fresh name directory seeded with the startup descriptors.
pub(super) fn startup_type_map() -> HashMap<String, &'static TypeDescriptor> {
    startup_descriptors()
        .into_iter()
        .map(|(name, descriptor)| (name.to_string(), descriptor))
        .collect()
}

impl<P: ProcessManager> ExecState<P> {
    /// Register a host-defined type descriptor (usually `T::descriptor()`
    /// for a `#[oxdock_type]` payload). Makes the name visible to `TYPES()`
    /// and valid for `LET $x: NAME` declarations carrying same-named
    /// payloads. Re-registering a name returns silently when the descriptor
    /// is identical; a different descriptor under a live name panics
    /// instead of aliasing two layouts.
    pub fn register_type(&mut self, descriptor: &'static TypeDescriptor) {
        if let Some(live) = self.types.get(descriptor.name) {
            if !std::ptr::eq(*live, descriptor) {
                panic!(
                    "type name `{}` already registered for a different descriptor",
                    descriptor.name
                );
            }
            return;
        }
        self.types.insert(descriptor.name.to_string(), descriptor);
    }

    /// True when `name` names a registered type (startup or host).
    pub fn is_known_type(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    /// Resolve a descriptor by name, or `None` when unregistered.
    pub fn describe_type(&self, name: &str) -> Option<&'static TypeDescriptor> {
        self.types.get(name).copied()
    }

    /// Every registered type name: startup descriptors first, then hosts in
    /// registration order.
    pub fn type_names(&self) -> Vec<String> {
        let mut names: Vec<String> = startup_descriptors()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();
        for name in self.types.keys() {
            if !names.contains(&name.to_string()) {
                names.push(name.clone());
            }
        }
        names
    }
}
