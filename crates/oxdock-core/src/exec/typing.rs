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

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use oxdock_parser::{
    ANY_TAG, MAX_GENERIC_DEPTH, Spelled, SpelledField, Step, StepKind, canonicalize_spelling,
    intern_composed, lookup_composed, parse_spelling,
};
use oxdock_process::ProcessManager;

use super::state::ExecState;

/// Check an options map against its declared keys: unknown keys fail
/// listing the known ones, so script typos fail fast instead of
/// silently ignored. Shared by generated extractors and hand-built
/// entries alike: both validate against the same `ParamOption`
/// statics the metadata renders, so the documented keys and the
/// enforced keys cannot drift apart. Per-key value checks stay with
/// the function (defaults, coercion, and cross-key rules are logic,
/// not shape).
pub fn check_options(
    map: &std::collections::BTreeMap<String, Value>,
    options: &[crate::ParamOption],
    func: &str,
) -> Result<()> {
    for key in map.keys() {
        if !options.iter().any(|known| known.name == key) {
            let known: Vec<&str> = options.iter().map(|known| known.name).collect();
            bail!(
                "{func}() unknown option '{key}' (expected: {})",
                known.join(", ")
            );
        }
    }
    Ok(())
}

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

    /// Every script alias name, sorted. Backs discovery listings
    /// and unknown-type errors alongside schemas.
    pub fn alias_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.type_aliases.keys().cloned().collect();
        names.sort();
        names
    }

    /// The canonical target spelling of one script alias, if defined.
    /// Backs `TYPE_DESCRIBE` so aliases introspect like named types.
    pub fn alias_target(&self, name: &str) -> Option<String> {
        self.type_aliases.get(name).cloned()
    }

    /// Resolve a source-level type name to its tag: builtins by closed
    /// match, customs through the type directory, named schemas through
    /// the schema directory, script aliases through the alias directory,
    /// and generic spellings (`LIST<MAP>`, `MAP<name: STRING>`) composed
    /// structurally with one interned entry per distinct spelling.
    /// Anything else bails listing every known type, schema, and alias.
    /// Single choke for every `LET`/`FUNC` declaration. Signature is
    /// stable: alias cycles and shared depth accounting thread
    /// internally, so all call sites gain generics with zero edits.
    pub fn resolve_tag(&self, name: &str) -> Result<TypeTag> {
        let canonical = canonicalize_spelling(name);
        if canonical.contains('<') {
            return self.resolve_spelling_text(&canonical, &mut Vec::new(), 0);
        }
        self.resolve_named(&canonical, &mut Vec::new(), 0)
    }

    /// Resolve one bare name through every directory. Alias targets
    /// loop back through the spelling path, so `LIST<PERSON>`
    /// composes; each alias hop costs 1 of the shared depth budget
    /// and repeats bail naming the cycle.
    fn resolve_named(
        &self,
        name: &str,
        resolving: &mut Vec<String>,
        depth: usize,
    ) -> Result<TypeTag> {
        if depth > MAX_GENERIC_DEPTH {
            bail!("type spelling exceeds maximum generic depth of {MAX_GENERIC_DEPTH}: '{name}'");
        }
        if name == "MAP" {
            bail!(
                "bare MAP is not a declaration type: write a shape (MAP<name: TYPE, ...>) or MAP<ANY> for maps with unknown keys"
            );
        }
        if name == "LIST" {
            bail!(
                "bare LIST is not a declaration type: write a shape (LIST<TYPE>) or LIST<ANY> for lists with unknown elements"
            );
        }
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
        if let Some(target) = self.type_aliases.get(name) {
            if resolving.contains(&name.to_string()) {
                resolving.push(name.to_string());
                bail!("type alias cycle: {}", resolving.join(" -> "));
            }
            resolving.push(name.to_string());
            let target = target.clone();
            let result = self.resolve_spelling_text(&target, resolving, depth + 1);
            resolving.pop();
            return result;
        }
        bail!(
            "unknown type '{name}'; known types: {} and schemas: {} and aliases: {}",
            self.type_names().join(", "),
            self.schema_names().join(", "),
            self.alias_names().join(", "),
        )
    }

    /// Resolve one canonical spelling that may carry `<...>` args:
    /// interned hit returns shared, else parse, build, and intern.
    fn resolve_spelling_text(
        &self,
        canonical: &str,
        resolving: &mut Vec<String>,
        depth: usize,
    ) -> Result<TypeTag> {
        if let Some(hit) = lookup_composed(canonical) {
            return Ok(*hit);
        }
        let spelled = parse_spelling(canonical)
            .map_err(|err| anyhow::anyhow!("invalid type spelling: {err}"))?;
        let tag = self.build_composed(&spelled, resolving, depth)?;
        Ok(*intern_composed(canonical.to_string(), tag))
    }

    /// Compose a parsed spelling into its structural tag. `LIST`
    /// takes exactly one bare argument, `MAP` takes named fields;
    /// anything else with arguments (or `MAP` with bare ones)
    /// bails naming the rule. Leaves resolve through the full
    /// directory, so schema and alias names nest freely.
    fn build_composed(
        &self,
        spelled: &Spelled,
        resolving: &mut Vec<String>,
        depth: usize,
    ) -> Result<TypeTag> {
        if depth > MAX_GENERIC_DEPTH {
            bail!(
                "type spelling exceeds maximum generic depth of {MAX_GENERIC_DEPTH}: '{spelled:?}'"
            );
        }
        match spelled {
            Spelled::Named(name) => self.resolve_named(name, resolving, depth),
            Spelled::Generic { name, args } => self.build_generic(name, args, resolving, depth),
        }
    }

    /// Compose one generic application. `LIST` takes exactly one
    /// bare argument, `MAP` takes named fields; any other head
    /// resolves bare first so the error names the real problem
    /// (`unknown type` vs. arguments on a scalar, custom,
    /// schema, or alias target). Leaf homes leak like the composed
    /// entry itself (one per distinct spelling; see
    /// `intern_composed`), localized here for the same reason.
    #[allow(clippy::disallowed_methods)]
    fn build_generic(
        &self,
        name: &str,
        args: &[SpelledField],
        resolving: &mut Vec<String>,
        depth: usize,
    ) -> Result<TypeTag> {
        if name != "LIST" && name != "MAP" {
            self.resolve_named(name, resolving, depth)?;
            bail!("'{name}' takes no type arguments");
        }
        if name == "LIST" {
            let [
                SpelledField {
                    name: None,
                    ty: element,
                    ..
                },
            ] = args
            else {
                bail!("LIST takes exactly one bare type argument");
            };
            let element = self.build_composed(element, resolving, depth + 1)?;
            return Ok(TypeTag::ListOf(Box::leak(Box::new(element))));
        }
        if name == "MAP"
            && let [
                SpelledField {
                    name: None,
                    ty: Spelled::Named(any),
                    ..
                },
            ] = args
            && any.as_str() == "ANY"
        {
            // Explicit unknown map: the only bare MAP spelling. Anything
            // else bare still rejects below, and plain `MAP` never
            // resolves (see `resolve_named`).
            return Ok(TypeTag::MapOf(&ANY_TAG));
        }
        let mut fields = Vec::with_capacity(args.len());
        for arg in args {
            let Some(field_name) = arg.name.as_ref() else {
                bail!("MAP fields require 'name: TYPE'; '{name}' takes no bare arguments");
            };
            let ty = self.build_composed(&arg.ty, resolving, depth + 1)?;
            let leaked: &'static str = Box::leak(field_name.clone().into_boxed_str());
            fields.push(Field {
                name: leaked,
                ty,
                docs: "",
                optional: arg.optional,
            });
        }
        Ok(TypeTag::Record(Box::leak(fields.into_boxed_slice())))
    }
}
/// Collect top-level `TYPE` alias definitions into the run's
/// alias directory: name to canonical target spelling. Runs
/// before the static pass so forward references resolve
/// run-wide. Duplicates and collisions with builtin, host,
/// schema, and top-level `FUNC` names are static errors
/// naming the offender. Nested `TYPE` steps cannot occur
/// (the parser rejects them); hand-built ones never resolve.
pub fn collect_type_aliases<P: ProcessManager>(
    steps: &[Step],
    exec: &ExecState<P>,
) -> Result<HashMap<String, String>> {
    let mut func_names = HashSet::new();
    for step in steps {
        if let StepKind::FuncDef { name, .. } = &step.kind {
            func_names.insert(name.clone());
        }
    }
    let mut aliases = HashMap::new();
    for step in steps {
        let StepKind::TypeAlias { name, target } = &step.kind else {
            continue;
        };
        if aliases.contains_key(name) {
            bail!("type alias '{name}' is already defined");
        }
        if TypeTag::builtin(name).is_some() {
            bail!("type alias '{name}' collides with a builtin type");
        }
        if exec.types.contains_key(name) {
            bail!("type alias '{name}' collides with registered host type '{name}'");
        }
        if exec.record_schemas.contains_key(name) {
            bail!("type alias '{name}' collides with registered record schema '{name}'");
        }
        if func_names.contains(name) {
            bail!("type alias '{name}' collides with function '{name}'");
        }
        aliases.insert(name.clone(), canonicalize_spelling(target));
    }
    Ok(aliases)
}

impl<P: ProcessManager> ExecState<P> {
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
            TypeTag::MapOf(values) => self.check_tag_known(what, values)?,
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
    /// registration order, then script aliases.
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
        for name in self.alias_names() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}
