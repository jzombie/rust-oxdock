//! Type-system integration for the executor.
//!
//! Name directories (which descriptor answers for `"TAG"`) live per
//! execution state: one `HashMap` seeded with the startup descriptors,
//! extended by the run's host descriptors. Words carry their own vtable,
//! so lifecycle never consults any table; only name resolution (declaration
//! checking, `TYPES()`, `TYPE_DESCRIBE`) reads the state map.

pub use oxdock_parser::{
    ANY_TYPE_NAME, Field, HANDLE_TYPE_NAME, LIST_TAG, LIST_TYPE_NAME, MAP_TAG, MAP_TYPE_NAME,
    OxDockType, TypeDescriptor, TypeTag, Value, ValuePayload, check_value, check_value_at,
    clone_boxed, clone_copy, clone_shared, drop_boxed, drop_noop, drop_shared, eq_boxed, eq_inline,
    eq_shared, fmt_boxed, fmt_inline, fmt_shared, load_inline, startup_descriptors, store_inline,
    type_anchor, unshare_boxed, unshare_inline, unshare_shared,
};

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use oxdock_parser::{
    ANY_TAG, MAX_GENERIC_DEPTH, Spelled, SpelledField, Step, StepKind, canonicalize_spelling,
    intern_rendered, lookup_rendered, parse_spelling,
};
use oxdock_process::ProcessManager;

use super::state::ExecState;

/// Check an options map against its declared keys: unknown keys fail
/// listing the known ones, so script typos fail fast instead of
/// silently ignored. Shared by generated extractors and hand-built
/// entries alike: both validate against the same `ParamOption`
/// statics the metadata renders, so the documented keys and the
/// enforced keys cannot drift apart. Present values check against
/// the declared `ParamOption.value` through the single shape path,
/// so a wrong-typed option fails here naming func, key, expected,
/// and actual instead of deeper in the body. Absent keys pass here:
/// defaults, coercion, and cross-key rules stay function logic.
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
    for (key, value) in map.iter() {
        let option = options
            .iter()
            .find(|known| known.name == *key)
            .expect("unknown keys rejected above");
        crate::check_value_at(&option.value, value, &format!("{func}() option '{key}'"))?;
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
    pub fn register_record_schema(&mut self, name: &'static str, fields: std::sync::Arc<[Field]>) {
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
    /// generic spellings (`LIST<MAP>`, `MAP<name: STRING>`) rendered
    /// structurally, and unions (`STRING|PIPE`, including inside
    /// generics) with one interned entry per distinct spelling.
    /// Anything else bails listing every known type, schema, and alias.
    /// Single choke for every `LET`/`FUNC` declaration. Signature is
    /// stable: alias cycles and shared depth accounting thread
    /// internally, so all call sites gain generics with zero edits.
    pub fn resolve_tag(&self, name: &str) -> Result<TypeTag> {
        let canonical = canonicalize_spelling(name);
        if canonical.contains('<') || canonical.contains('|') {
            return self.resolve_spelling_text(&canonical, &mut Vec::new(), 0);
        }
        self.resolve_named(&canonical, &mut Vec::new(), 0)
    }

    /// Resolve one bare name through every directory. Alias targets
    /// loop back through the spelling path, so `LIST<PERSON>`
    /// combines; each alias hop costs 1 of the shared depth budget
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
        if name == MAP_TYPE_NAME {
            bail!(
                "bare {MAP_TYPE_NAME} is not a declaration type: write a shape ({MAP_TYPE_NAME}<name: TYPE, ...>) or {MAP_TYPE_NAME}<{ANY_TYPE_NAME}> for maps with unknown keys"
            );
        }
        if name == LIST_TYPE_NAME {
            bail!(
                "bare {LIST_TYPE_NAME} is not a declaration type: write a shape ({LIST_TYPE_NAME}<TYPE>) or {LIST_TYPE_NAME}<{ANY_TYPE_NAME}> for lists with unknown elements"
            );
        }
        if name == HANDLE_TYPE_NAME {
            bail!(
                "bare {HANDLE_TYPE_NAME} is not a declaration type: write {HANDLE_TYPE_NAME}<T> for a known payload or {HANDLE_TYPE_NAME}<{ANY_TYPE_NAME}> for an unknown one"
            );
        }
        if let Some(tag) = TypeTag::builtin(name) {
            return Ok(tag);
        }
        if let Some(descriptor) = self.types.get(name) {
            return Ok(TypeTag::Custom(descriptor));
        }
        if let Some(fields) = self.record_schemas.get(name) {
            let tag = TypeTag::Record(fields.clone());
            self.check_tag_known("declaration", &tag, false)?;
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
        if let Some(hit) = lookup_rendered(canonical) {
            return Ok(hit);
        }
        let spelled = parse_spelling(canonical)
            .map_err(|err| anyhow::anyhow!("invalid type spelling: {err}"))?;
        let tag = self.build_rendered(&spelled, resolving, depth)?;
        Ok(intern_rendered(canonical.to_string(), tag))
    }

    /// Build a parsed spelling into its structural tag. `LIST`
    /// takes exactly one bare argument, `MAP` takes named fields;
    /// anything else with arguments (or `MAP` with bare ones)
    /// bails naming the rule. Leaves resolve through the full
    /// directory, so schema and alias names nest freely. Union
    /// members resolve the same way, so unions nest inside aliases,
    /// generics, and each other through one shared normalizer.
    fn build_rendered(
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
            Spelled::Union(members) => {
                let mut resolved = Vec::with_capacity(members.len());
                for member in members {
                    resolved.push(self.build_rendered(member, resolving, depth)?);
                }
                finish_union(resolved)
            }
        }
    }

    /// Build one generic application. `LIST` takes exactly one
    /// bare argument, `MAP` takes named fields; any other head
    /// resolves bare first so the error names the real problem
    /// (`unknown type` vs. arguments on a scalar, custom,
    /// schema, or alias target). Leaf homes leak like the rendered
    /// entry itself (one per distinct spelling; see
    /// `intern_rendered`), localized here for the same reason.
    #[allow(clippy::disallowed_methods)]
    fn build_generic(
        &self,
        name: &str,
        args: &[SpelledField],
        resolving: &mut Vec<String>,
        depth: usize,
    ) -> Result<TypeTag> {
        if name != LIST_TYPE_NAME && name != MAP_TYPE_NAME && name != HANDLE_TYPE_NAME {
            self.resolve_named(name, resolving, depth)?;
            bail!("'{name}' takes no type arguments");
        }
        if name == HANDLE_TYPE_NAME {
            let [
                SpelledField {
                    name: None,
                    ty: payload,
                    ..
                },
            ] = args
            else {
                bail!("{HANDLE_TYPE_NAME} takes exactly one bare type argument");
            };
            let payload = self.build_rendered(payload, resolving, depth + 1)?;
            return Ok(TypeTag::HandleOf(std::sync::Arc::new(payload)));
        }
        if name == LIST_TYPE_NAME {
            let [
                SpelledField {
                    name: None,
                    ty: element,
                    ..
                },
            ] = args
            else {
                bail!("{LIST_TYPE_NAME} takes exactly one bare type argument");
            };
            let element = self.build_rendered(element, resolving, depth + 1)?;
            return Ok(TypeTag::ListOf(std::sync::Arc::new(element)));
        }
        if name == MAP_TYPE_NAME
            && let [
                SpelledField {
                    name: None,
                    ty: Spelled::Named(any),
                    ..
                },
            ] = args
            && any.as_str() == ANY_TYPE_NAME
        {
            // Explicit unknown map: the only bare MAP spelling. Anything
            // else bare still rejects below, and plain `MAP` never
            // resolves (see `resolve_named`).
            return Ok(TypeTag::MapOf(std::sync::Arc::new(ANY_TAG)));
        }
        let mut fields = Vec::with_capacity(args.len());
        for arg in args {
            let Some(field_name) = arg.name.as_ref() else {
                bail!(
                    "{MAP_TYPE_NAME} fields require 'name: TYPE'; '{name}' takes no bare arguments"
                );
            };
            let ty = self.build_rendered(&arg.ty, resolving, depth + 1)?;
            fields.push(Field {
                name: field_name.clone(),
                ty,
                docs: "",
                optional: arg.optional,
            });
        }
        Ok(TypeTag::Record(std::sync::Arc::from(fields)))
    }
}

/// Normalize resolved union members into one tag: flatten nested
/// unions, absorb on `ANY`, deduplicate, collapse lone survivors.
/// Members keep written order while every comparison stays
/// order-free, so all spellings behave identically. The tag owns
/// everything through `Arc`: shared on clone, freed with the last
/// owner, never leaked.
fn finish_union(members: Vec<TypeTag>) -> Result<TypeTag> {
    let mut flat: Vec<TypeTag> = Vec::with_capacity(members.len());
    for member in members {
        // Nested unions splice flat: members never nest, so every
        // comparison stays one level deep by construction.
        if let TypeTag::Union(_, nested) = member {
            flat.extend(nested.iter().cloned());
        } else {
            flat.push(member);
        }
    }
    if flat.is_empty() {
        bail!("empty union: no member shapes to admit");
    }
    // `ANY` absorbs the set: admitting everything is `ANY`.
    if flat.iter().any(|member| matches!(member, TypeTag::Any)) {
        return Ok(TypeTag::Any);
    }
    // Full (not just consecutive) deduplication: order-free
    // comparisons make every permutation behave identically.
    let mut unique: Vec<TypeTag> = Vec::with_capacity(flat.len());
    for member in flat {
        if !unique
            .iter()
            .any(|seen| super::static_check::tags_equal(seen, &member))
        {
            unique.push(member);
        }
    }
    if unique.len() == 1 {
        return Ok(unique.pop().expect("single survivor checked above"));
    }
    Ok(TypeTag::union(unique))
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
    /// Single choke for declaration validity: known customs plus
    /// the VOID position rule. VOID is legal only as a bare function
    /// return (`top_void` covers exactly that call) or as the direct
    /// payload of HANDLE (`HANDLE<VOID>`). Every other position is a
    /// static error naming the site: variable, parameter, record
    /// field, union member, container element, or anything nested
    /// deeper. One recursion serves all declaration sites, so the
    /// rule cannot drift between LET, params, returns, schemas, and
    /// aliases: resolve first, then validate here.
    fn check_tag_known(&self, what: &str, tag: &TypeTag, top_void: bool) -> Result<()> {
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
            TypeTag::Void if !top_void => {
                bail!(
                    "{what} declares VOID outside a return annotation: VOID is only a return type (or HANDLE<VOID> payload)"
                );
            }
            TypeTag::Void => {}
            TypeTag::ListOf(element) => self.check_tag_known(what, element, false)?,
            TypeTag::HandleOf(payload) => self.check_tag_known(what, payload, true)?,
            TypeTag::MapOf(values) => self.check_tag_known(what, values, false)?,
            TypeTag::Record(fields) => {
                for field in fields.iter() {
                    self.check_tag_known(what, &field.ty, false)?;
                }
            }
            // Union members recurse like shaped tags: every member
            // must name a known type, so no unknown hides in a set.
            // A VOID member is never admitted: unions of handles stay
            // inside the payload (HANDLE<INT|STRING>), never at the
            // root or beside a value type.
            TypeTag::Union(_, members) => {
                for member in members.iter() {
                    self.check_tag_known(what, member, false)?;
                }
            }
            // Bare words are variants and cannot be misspelled: every
            // builtin is listed so a new word kind fails here until
            // its directory rule is explicit.
            TypeTag::String
            | TypeTag::Int
            | TypeTag::Float
            | TypeTag::Bool
            | TypeTag::List
            | TypeTag::Map
            | TypeTag::Pipe
            | TypeTag::Handle
            | TypeTag::Duration
            | TypeTag::Path
            | TypeTag::Semaphore
            | TypeTag::Permit
            | TypeTag::Any => {}
        }
        Ok(())
    }

    /// Declaration-site gate for everything that is not a function
    /// return: LET bindings, parameters, schemas, aliases at use.
    /// Single choke so the VOID position rule cannot drift between
    /// the static checker and runtime definition paths.
    pub fn check_declaration_tag(&self, what: &str, tag: &TypeTag) -> Result<()> {
        self.check_tag_known(what, tag, false)
    }

    /// Return-position gate: bare VOID is legal here (and only
    /// here, besides HANDLE payloads). Same single recursion as
    /// every other declaration site.
    pub fn check_return_tag(&self, what: &str, tag: &TypeTag) -> Result<()> {
        self.check_tag_known(what, tag, true)
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
                for (position, param) in params.iter().enumerate() {
                    if let Some(expected) = param.param_type.as_ref() {
                        let what = format!("param '{}' of '{}'", param.name, meta.name);
                        Self::check_tag_known(self, &what, expected, false)?;
                    }
                    if param.param_type.is_none() && param.allowed.is_some() {
                        anyhow::bail!(
                            "param '{}' of '{}' declares values without a type",
                            param.name,
                            meta.name,
                        );
                    }
                    if param.optional {
                        // Omission fills an empty MAP, so only trailing
                        // MAP params qualify: anything else would fill a
                        // value the caller never chose.
                        let trails = params[position..].iter().all(|later| later.optional);
                        if !trails {
                            anyhow::bail!(
                                "param '{}' of '{}' is optional before a required param",
                                param.name,
                                meta.name,
                            );
                        }
                        if !matches!(param.param_type, Some(TypeTag::Map)) {
                            anyhow::bail!(
                                "param '{}' of '{}' is optional without a MAP type",
                                param.name,
                                meta.name,
                            );
                        }
                    }
                }
            }
            if let Some(returns) = meta.returns.as_ref() {
                let what = format!("return of '{}'", meta.name);
                Self::check_tag_known(self, &what, returns, true)?;
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
