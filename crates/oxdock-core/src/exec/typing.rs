//! Type-system integration for the executor.
//!
//! Name directories (which descriptor answers for `"TAG"`) live per
//! execution state: one `HashMap` seeded with the startup descriptors,
//! extended by the run's host descriptors. Words carry their own vtable,
//! so lifecycle never consults any table; only name resolution (declaration
//! checking, `TYPES()`, `TYPE_DESCRIBE`) reads the state map.

pub use oxdock_parser::{
    Field, LIST_TAG, MAP_TAG, OxDockType, TypeDescriptor, TypeTag, Value, ValuePayload,
    check_value, check_value_at, clone_boxed, clone_copy, clone_shared, drop_boxed, drop_noop,
    drop_shared, eq_boxed, eq_inline, eq_shared, fmt_boxed, fmt_inline, fmt_shared, load_inline,
    startup_descriptors, store_inline, type_anchor, unshare_boxed, unshare_inline, unshare_shared,
};

use std::collections::HashMap;

use anyhow::{Result, bail};
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

    /// Register a named record schema for shaped MAP bindings (usually
    /// alongside the module that produces them). Makes the name valid
    /// for `LET $x: NAME` declarations carrying conforming maps. A
    /// different field table under a live name panics instead of
    /// aliasing two shapes.
    pub fn register_record_schema(&mut self, name: &'static str, fields: &'static [Field]) {
        if let Some(live) = self.record_schemas.get(name) {
            if live.len() != fields.len()
                || live
                    .iter()
                    .zip(fields.iter())
                    .any(|(a, b)| a.name != b.name)
            {
                panic!("record schema `{name}` already registered with different fields");
            }
            return;
        }
        self.record_schemas.insert(name.to_string(), fields);
    }

    /// Every registered schema name, sorted. Backs discovery listings.
    pub fn schema_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.record_schemas.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve a source-level type name to its tag: builtins by closed
    /// match, customs through the type directory, named schemas through
    /// the schema directory. Anything else bails listing every known
    /// type and schema. Single choke for every `LET`/`FUNC` declaration.
    pub fn resolve_tag(&self, name: &str) -> Result<TypeTag> {
        if let Some(tag) = TypeTag::builtin(name) {
            return Ok(tag);
        }
        if let Some(descriptor) = self.types.get(name) {
            return Ok(TypeTag::Custom(descriptor));
        }
        if let Some(fields) = self.record_schemas.get(name) {
            let tag = TypeTag::Record(fields);
            self.check_tag_known("declaration", &tag)?;
            return Ok(tag);
        }
        bail!(
            "unknown type '{name}'; known types: {} and schemas: {}",
            self.type_names().join(", "),
            self.schema_names().join(", "),
        )
    }

    /// Check one tag against the directories: customs must name a
    /// registered descriptor, shaped tags recurse into fields and
    /// elements, builtins are variants and cannot be misspelled.
    /// Unknown names bail naming the use site and every known type.
    fn check_tag_known(&self, what: &str, tag: &TypeTag) -> Result<()> {
        match tag {
            TypeTag::Custom(descriptor) => {
                if !self.types.contains_key(descriptor.name) {
                    bail!(
                        "{what} declares unregistered type '{}'; known types: {}",
                        descriptor.name,
                        self.type_names().join(", ")
                    );
                }
            }
            TypeTag::ListOf(element) => self.check_tag_known(what, element)?,
            TypeTag::Record(fields) => {
                for field in *fields {
                    self.check_tag_known(what, &field.ty)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Fail fast when any registered function declares param or return
    /// type names outside the type directory. Runs once per run before
    /// the first step executes: param arity and `LET` coercion already
    /// check values at use time, but a typo'd `returns` label otherwise
    /// lives forever as a lie in `DESCRIBE` output and generated
    /// references. Unknown names bail listing every known type. A
    /// closed `allowed` set without a type is rejected too: the set
    /// only means something alongside the `STRING` check that enforces
    /// it, so hand-built entries cannot render a set they never check.
    pub fn validate_function_type_tags(&self) -> Result<()> {
        for meta in self.functions.all_metas() {
            if let Some(params) = meta.params.as_deref() {
                for param in params {
                    if let Some(expected) = param.param_type.as_ref() {
                        let what = format!("param '{}' of '{}'", param.name, meta.name);
                        Self::check_tag_known(self, &what, expected)?;
                    }
                    if param.param_type.is_none() && param.allowed.is_some() {
                        anyhow::bail!(
                            "param '{}' of '{}' declares values without a type",
                            param.name,
                            meta.name,
                        );
                    }
                }
            }
            if let Some(returns) = meta.returns.as_ref() {
                let what = format!("return of '{}'", meta.name);
                Self::check_tag_known(self, &what, returns)?;
            }
        }
        Ok(())
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
