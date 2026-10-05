//! Host export macros for DSL native and host functions (`#[oxdock_func]`)
//! and host-defined types (`#[oxdock_type]`).
//!
//! Writing a registry entry by hand means a `FuncMeta` literal, an arity
//! check, and `Vec<Value>` unpacking per function. The attribute macro
//! derives all of that from the Rust signature plus doc comments; the
//! runnable example lives in the `oxdock-core` `Engine` docs. It is not
//! duplicated here: a proc-macro crate cannot depend on its own downstream
//! consumers, and an uncompiled duplicate would rot.
//!
//! Storage modes for `#[oxdock_type]` (the annotated struct IS the payload):
//!
//! - Default (heap): the word holds a thin pointer to an owned `Box<T>`,
//!   one allocation per word. Requires `Clone + PartialEq + Display +
//!   Debug + Send + Sync + 'static`.
//! - `inline`: the word holds `T`'s bytes directly in the 64-bit payload,
//!   zero allocation. Requires `Copy` plus the heap bounds, and
//!   `size_of::<T>() <= 8` (checked at mint time).
//!
//! The startup-registered types use the identical macro: integers, floats,
//! booleans, and handles are `inline` payloads; text, lists, maps, paths,
//! durations, and pipes are heap payloads.
//!
//! Contract for the annotated function:
//!
//! - `#[oxdock_func(pure)]`: no context parameter. The function runs on both
//!   the AST and the compiled RPN math paths. Usable parameter types are
//!   `Value`, `String`, `i64`, `f64`, and `bool`.
//! - `#[oxdock_func]`: first parameter must be `cx: &mut StepCtx<P>` (any
//!   generic name). The function runs on the AST path with full step context.
//!   Add `rpn` to also run on the compiled math path (`GLOB`, `LOAD_TOML`,
//!   and `LOAD_JSON` opt in; everything else stays AST-only).
//! - Return type must be `Result<Value>` (spelled via any `Result` alias).
//! - The DSL name defaults to the uppercased Rust name (`load_toml` becomes
//!   `LOAD_TOML`); override with `name = "..."`. The declared return type
//!   comes from `returns = TypeTag::...` (a `TypeTag` expression) and
//!   defaults to none.
//! - The first doc-comment line becomes `FuncMeta.summary`; the full doc
//!   text becomes `FuncMeta.docs`. Override the summary with `summary = "..."`.
//!
//! The macro keeps the original function untouched (still directly callable
//! and unit testable) and emits one sibling next to it: a registration
//! marker struct named after the function in `UpperCamelCase` (`make_tag`
//! becomes `MakeTag`) implementing `::oxdock_core::OxDockFn`. Pass the
//! marker into a `HostModule` for `Engine::register_module`. Generated code names `::oxdock_core::`
//! and `::anyhow::` paths, so using crates need both as direct
//! dependencies.
//!
//! Types map to DSL parameters as follows: `String` accepts STRING only;
//! `i64` accepts `Int`, integral finite `Float`, and trimmed integer strings;
//! `f64` accepts finite `Float`, `Int`, and trimmed numeric strings; `bool`
//! accepts `Bool` and `"true"`/`"false"` strings; `Value` accepts anything.
//! Arity failures report `{NAME}() expects {n} argument(s), got {m}`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, ItemFn, Lit, LitStr, Meta, Pat, Token, Type};

/// Host export macro for DSL native and host functions. See the crate docs for the
/// full contract: `#[oxdock_func]` for stateful functions taking
/// `cx: &mut StepCtx<P>` first, `#[oxdock_func(pure)]` for pure scalar
/// functions. Derives a registration marker (`UpperCamelCase` of the function
/// name: `workspace_members` becomes `WorkspaceMembers`) implementing
/// `OxDockFn`: group its marker into a `HostModule` for `Engine::register_module`.
#[proc_macro_attribute]
pub fn oxdock_func(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_oxdock_func(attr, item) {
        Ok(expanded) => expanded.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Host export macro for DSL host-defined types. See the crate docs for the full
/// contract: `#[oxdock_type(name = "EMBEDDING")]` on the payload struct derives
/// an `OxDockType` implementation holding the `TypeDescriptor` from the name
/// plus doc comments. `#[oxdock_type(name = "ENTITY", inline)]` selects the
/// zero-allocation payload path for `Copy` scalars that fit in 64 bits.
/// `#[oxdock_type(name = "LIST", shared)]` selects the shared `Arc` path:
/// clones bump a refcount instead of deep-copying (for immutable container
/// payloads; mint with `Value::mint_heap_shared`).
/// Register with `Engine::register_type::<Payload>()`. Values ride words
/// minted with `Value::mint_heap` / `Value::mint_heap_shared` /
/// `Value::mint_inline`, and are read back with `Value::read_heap` /
/// `Value::read_inline`.
#[proc_macro_attribute]
pub fn oxdock_type(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_oxdock_type(attr, item) {
        Ok(expanded) => expanded.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_oxdock_func(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream2> {
    let options = syn::parse::<FuncOptions>(attr)?;
    let func = syn::parse::<ItemFn>(item)?;
    expand_func(options, func)
}

/// A declared return type is always a `TypeTag` expression, resolved by
/// the compiler. String labels are rejected: pre-release software takes
/// the breaking change now so unchecked strings can never slip through.
#[derive(Default)]
struct FuncOptions {
    pure: bool,
    rpn: bool,
    name: Option<String>,
    returns: Option<syn::Expr>,
    summary: Option<String>,
}

impl Parse for FuncOptions {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut options = FuncOptions::default();
        while !input.is_empty() {
            if input.peek(syn::Ident) && !peek_key_value(input) {
                let flag: syn::Ident = input.parse()?;
                if flag == "pure" {
                    options.pure = true;
                } else if flag == "rpn" {
                    options.rpn = true;
                } else {
                    return Err(syn::Error::new(
                        flag.span(),
                        "unknown oxdock_func flag; expected pure or rpn",
                    ));
                }
            } else if input.peek(syn::Ident) {
                let key: syn::Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                if key == "returns" {
                    let value: syn::Expr = input.parse()?;
                    options.returns = Some(parse_returns(value)?);
                    if input.peek(Token![,]) {
                        input.parse::<Token![,]>()?;
                    }
                    continue;
                }
                let value: syn::LitStr = input.parse()?;
                if key == "name" {
                    options.name = Some(value.value());
                } else if key == "summary" {
                    options.summary = Some(value.value());
                } else {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown oxdock_func option; expected pure, rpn, name, returns, or summary",
                    ));
                }
            } else {
                return Err(input.error("expected pure or key = \"value\""));
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(options)
    }
}

fn peek_key_value(input: ParseStream) -> bool {
    if !input.peek(syn::Ident) {
        return false;
    }
    let fork = input.fork();
    if fork.parse::<syn::Ident>().is_err() {
        return false;
    }
    fork.peek(Token![=])
}

/// Parse a `returns` attribute value: must be a `TypeTag` expression
/// (`TypeTag::String`, `TypeTag::Custom(FooTag::descriptor())`,
/// `TypeTag::Record(&FIELDS)`). String literals fail here, so custom
/// and shaped types can never slip through as unchecked strings.
fn parse_returns(value: syn::Expr) -> syn::Result<syn::Expr> {
    if let syn::Expr::Lit(lit) = &value
        && let syn::Lit::Str(text) = &lit.lit
    {
        return Err(syn::Error::new(
            text.span(),
            "returns must be a TypeTag expression, e.g. returns = TypeTag::String; string labels are rejected",
        ));
    }
    Ok(value)
}

/// A DSL-facing parameter: binding identifier, mapped type info, an
/// optional closed value set, and optional options keys. `allowed`
/// comes from a `#[values(...)]` attribute and is valid only on
/// `String` parameters; `options` comes from `#[options(...)]` and is
/// valid only on `MAP` parameters: the same token list feeds the
/// metadata, so the documented set cannot rot apart from the check.
struct Param {
    ident: syn::Ident,
    kind: ParamKind,
    allowed: Option<Vec<String>>,
    options: Option<Vec<OptSpec>>,
    docs: String,
}

/// One `#[options(...)]` entry (`name[?]: TYPE [= default]`): a known
/// key of a `MAP` options parameter with its value type, whether
/// callers must pass it, and the rendered default for optional keys.
/// Per-key prose arrives separately from the `# Options` doc section.
struct OptSpec {
    name: String,
    optional: bool,
    ty: TokenStream2,
    default: Option<String>,
}

#[derive(Clone)]
enum ParamKind {
    Value,
    String,
    Int,
    Float,
    Bool,
    Pipe,
    Semaphore,
    Custom(Box<Type>),
    Shaped(Shape),
}

/// A composable shape: leaves are words, composites nest freely.
/// Resolution is recursive over the Rust spelling, so no combination
/// is ever enumerated: `Vec<Vec<BTreeMap<String, Value>>>` works the
/// day someone writes it. Scalar numerics never nest (their
/// extractors coerce, which has no meaning inside a strict shape
/// check). Any-valued shapes collapse to their bare word: there is
/// nothing to check below them.
#[derive(Clone)]
enum Shape {
    Str,
    List,
    Map,
    ListOf(Box<Shape>),
}

impl ParamKind {
    fn type_path(&self) -> TokenStream2 {
        match self {
            // `Value` renders `ANY`: the extractor accepts every word,
            // so the tag is vacuously honest instead of a bare hole.
            ParamKind::Value => quote! { Some(::oxdock_core::TypeTag::Any) },
            ParamKind::String => quote! { Some(::oxdock_core::TypeTag::String) },
            ParamKind::Int => quote! { Some(::oxdock_core::TypeTag::Int) },
            ParamKind::Float => quote! { Some(::oxdock_core::TypeTag::Float) },
            ParamKind::Bool => quote! { Some(::oxdock_core::TypeTag::Bool) },
            // `PipeHandle` renders `PIPE`: the extractor below accepts
            // only pipe words, so metadata, check, and signature agree.
            ParamKind::Pipe => quote! { Some(::oxdock_core::TypeTag::Pipe) },
            // `Arc<SemaphoreState>` renders `SEMAPHORE` for the same
            // reason: the extractor is the `as_semaphore` read.
            ParamKind::Semaphore => quote! { Some(::oxdock_core::TypeTag::Semaphore) },
            // An `OxDockType` payload renders its registered name: the
            // extractor reads it back through the same descriptor, so
            // custom handles enforce exactly what they advertise.
            ParamKind::Custom(ty) => quote! {
                Some(::oxdock_core::TypeTag::Custom(
                    <#ty as ::oxdock_core::OxDockType>::descriptor(),
                ))
            },
            ParamKind::Shaped(shape) => {
                let tag = Self::shape_tag(shape);
                quote! { Some(#tag) }
            }
        }
    }

    /// Metadata tag for a shape, composed recursively: the same tree
    /// the extractor checks, so documentation and enforcement share
    /// one source by construction. Emits a value; check sites borrow
    /// it (constant promotion makes every nesting level `'static`).
    fn shape_tag(shape: &Shape) -> TokenStream2 {
        match shape {
            Shape::Str => quote! { ::oxdock_core::TypeTag::String },
            // Bare collections render their unknown shape (`LIST<ANY>`,
            // `MAP<ANY>`): the extractor accepts any word of the kind,
            // and the tag says exactly that instead of a bare word.
            Shape::List => quote! { ::oxdock_core::TypeTag::ListOf(&::oxdock_core::TypeTag::Any) },
            Shape::Map => quote! { ::oxdock_core::TypeTag::MapOf(&::oxdock_core::TypeTag::Any) },
            Shape::ListOf(inner) => {
                let inner_tag = Self::shape_tag(inner);
                quote! { ::oxdock_core::TypeTag::ListOf(&#inner_tag) }
            }
        }
    }

    /// Owned-value conversion for a shape that just passed
    /// `check_value_at`: every `expect` names the check that makes it
    /// infallible. Element closures shadow `__e` per level; each
    /// receiver binds outward, so shadowing is correct by scoping.
    fn shape_narrow(shape: &Shape, value: &TokenStream2) -> TokenStream2 {
        match shape {
            Shape::Str => {
                quote! { #value.as_str().expect("check_value_at enforces STRING").to_string() }
            }
            Shape::List => {
                quote! { #value.as_list().expect("check_value_at enforces LIST").clone() }
            }
            Shape::Map => {
                quote! { #value.as_map().expect("check_value_at enforces MAP").clone() }
            }
            Shape::ListOf(inner) => {
                let elem = ParamKind::shape_narrow(inner, &quote! { __e });
                quote! {
                    #value.as_list().expect("check_value_at enforces LIST").iter().map(|__e| #elem).collect::<Vec<_>>()
                }
            }
        }
    }

    /// Emitted extractor: turns the next `::oxdock_core::Value` word into
    /// the Rust type, bailing with a `{NAME}()`-prefixed message on
    /// mismatch. Reads go through word accessors; words are never
    /// destructured. A closed `allowed` set (from `#[values(...)]`)
    /// adds a membership check generated from the same tokens that
    /// feed the metadata, so the check and the documented set cannot
    /// drift apart.
    fn extractor(
        &self,
        param: &syn::Ident,
        dsl_name: &str,
        allowed: Option<&[String]>,
    ) -> TokenStream2 {
        match self {
            ParamKind::Value => quote! {
                __oxdock_values.next().expect("arity checked above")
            },
            ParamKind::String => {
                let membership = match allowed {
                    Some(values) => {
                        let lits: Vec<LitStr> = values
                            .iter()
                            .map(|value| LitStr::new(value, proc_macro2::Span::call_site()))
                            .collect();
                        let joined = values.join(", ");
                        quote! {
                            match s {
                                #(#lits => {},)*
                                _ => ::anyhow::bail!(
                                    "{}() argument `${}` must be one of: {}, got {s:?}",
                                    #dsl_name,
                                    stringify!(#param),
                                    #joined,
                                ),
                            }
                        }
                    }
                    None => quote! {},
                };
                quote! {
                    match __oxdock_values.next().expect("arity checked above").as_str() {
                        Some(s) => {
                            #membership
                            s.to_string()
                        }
                        None => ::anyhow::bail!(
                            "{}() argument `${}` must be a STRING",
                            #dsl_name,
                            stringify!(#param),
                        ),
                    }
                }
            }
            ParamKind::Int => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    if let Some(n) = __oxdock_v.as_i64() {
                        n
                    } else if let Some(f) = __oxdock_v.as_f64() {
                        if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                            f as i64
                        } else {
                            ::anyhow::bail!(
                                "{}() argument `${}` must be an integer value, found FLOAT ({f:?})",
                                #dsl_name,
                                stringify!(#param),
                            )
                        }
                    } else if let Some(s) = __oxdock_v.as_str() {
                        s.trim().parse::<i64>().map_err(|_| {
                            ::anyhow::anyhow!(
                                "{}() argument `${}` must be an integer string, found {s:?}",
                                #dsl_name,
                                stringify!(#param),
                            )
                        })?
                    } else {
                        ::anyhow::bail!(
                            "{}() argument `${}` must be an Int, Float, or String, found {__oxdock_v:?}",
                            #dsl_name,
                            stringify!(#param),
                        )
                    }
                }
            },
            ParamKind::Float => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    if let Some(f) = __oxdock_v.as_f64() {
                        if f.is_finite() {
                            f
                        } else {
                            ::anyhow::bail!(
                                "{}() argument `${}` must be finite, found FLOAT ({f:?})",
                                #dsl_name,
                                stringify!(#param),
                            )
                        }
                    } else if let Some(n) = __oxdock_v.as_i64() {
                        n as f64
                    } else if let Some(s) = __oxdock_v.as_str() {
                        let parsed: f64 = s.trim().parse().map_err(|_| {
                            ::anyhow::anyhow!(
                                "{}() argument `${}` must be a numeric string, found {s:?}",
                                #dsl_name,
                                stringify!(#param),
                            )
                        })?;
                        if parsed.is_finite() {
                            parsed
                        } else {
                            ::anyhow::bail!(
                                "{}() argument `${}` must be finite, found {s:?}",
                                #dsl_name,
                                stringify!(#param),
                            )
                        }
                    } else {
                        ::anyhow::bail!(
                            "{}() argument `${}` must be an Int, Float, or String, found {__oxdock_v:?}",
                            #dsl_name,
                            stringify!(#param),
                        )
                    }
                }
            },
            ParamKind::Bool => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    if let Some(b) = __oxdock_v.as_bool() {
                        b
                    } else if let Some(s) = __oxdock_v.as_str() {
                        match s.trim() {
                            "true" => true,
                            "false" => false,
                            _ => ::anyhow::bail!(
                                "{}() argument `${}` must be a Bool, found {s:?}",
                                #dsl_name,
                                stringify!(#param),
                            ),
                        }
                    } else {
                        ::anyhow::bail!(
                            "{}() argument `${}` must be a Bool or String, found {__oxdock_v:?}",
                            #dsl_name,
                            stringify!(#param),
                        )
                    }
                }
            },
            ParamKind::Shaped(shape) => {
                let tag = ParamKind::shape_tag(shape);
                let narrow = ParamKind::shape_narrow(shape, &quote! { __oxdock_v });
                quote! {
                    {
                        let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                        ::oxdock_core::check_value_at(
                            &(#tag),
                            &__oxdock_v,
                            &format!("{}() argument `${}`", #dsl_name, stringify!(#param)),
                        )?;
                        #narrow
                    }
                }
            }
            ParamKind::Pipe => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    match __oxdock_v.as_pipe_handle() {
                        Some(__oxdock_h) => __oxdock_h,
                        None => ::anyhow::bail!(
                            "{}() argument `${}` must be a PIPE, got {}",
                            #dsl_name,
                            stringify!(#param),
                            __oxdock_v.type_name(),
                        ),
                    }
                }
            },
            ParamKind::Semaphore => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    match __oxdock_v.as_semaphore() {
                        Some(__oxdock_h) => __oxdock_h,
                        None => ::anyhow::bail!(
                            "{}() argument `${}` must be a SEMAPHORE, got {}",
                            #dsl_name,
                            stringify!(#param),
                            __oxdock_v.type_name(),
                        ),
                    }
                }
            },
            ParamKind::Custom(ty) => quote! {
                {
                    let __oxdock_v = __oxdock_values.next().expect("arity checked above");
                    match __oxdock_v
                        .read_heap::<#ty>(<#ty as ::oxdock_core::OxDockType>::descriptor())
                    {
                        Some(__oxdock_h) => __oxdock_h.clone(),
                        None => ::anyhow::bail!(
                            "{}() argument `${}` expects a {} value, got {}",
                            #dsl_name,
                            stringify!(#param),
                            <#ty as ::oxdock_core::OxDockType>::descriptor().name,
                            __oxdock_v.type_name(),
                        ),
                    }
                }
            },
        }
    }
}

fn param_kind(ty: &Type) -> syn::Result<ParamKind> {
    if let Type::Path(path) = ty
        && let Some(last) = path.path.segments.last()
    {
        match last.ident.to_string().as_str() {
            "Value" => return Ok(ParamKind::Value),
            "String" => return Ok(ParamKind::String),
            "i64" => return Ok(ParamKind::Int),
            "f64" => return Ok(ParamKind::Float),
            "bool" => return Ok(ParamKind::Bool),
            "PipeHandle" => return Ok(ParamKind::Pipe),
            "Arc" => {
                // Shared semaphore backends travel as `Arc<SemaphoreState>`,
                // read back with `as_semaphore` like pipes.
                if let syn::PathArguments::AngleBracketed(args) = &last.arguments
                    && args.args.len() == 1
                    && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
                    && let Type::Path(inner_path) = inner
                    && let Some(inner_last) = inner_path.path.segments.last()
                    && inner_last.ident == "SemaphoreState"
                    && inner_last.arguments.is_empty()
                {
                    return Ok(ParamKind::Semaphore);
                }
            }
            "Vec" => {
                if let Some(shape) = list_shape(last) {
                    return Ok(ParamKind::Shaped(shape));
                }
            }
            "BTreeMap" => {
                if let Some(shape) = map_shape(last) {
                    return Ok(ParamKind::Shaped(shape));
                }
            }
            _ => {
                // Custom handle payload: any other plain (argument-less)
                // type names an `OxDockType` payload read back through
                // its descriptor. Generic and reference spellings keep
                // the friendly error below. A misspelled builtin still
                // fails compilation inside the re-emitted function, so
                // the typo surfaces at the definition site either way.
                if last.arguments.is_empty() {
                    return Ok(ParamKind::Custom(Box::new(ty.clone())));
                }
            }
        }
    }
    Err(syn::Error::new_spanned(
        ty,
        "unsupported oxdock_func parameter type; expected Value, String, i64, f64, bool, PipeHandle, Arc<SemaphoreState>, Vec<...>, or BTreeMap<String, Value>",
    ))
}

/// Resolve `Vec<...>` to a shape: `Vec<Value>` collapses to bare
/// `List` (nothing to check below the word), anything else composes
/// `ListOf` over the element shape. Scalar numerics are rejected:
/// their extractors coerce, which has no meaning inside strict shape
/// checking.
fn list_shape(segment: &syn::PathSegment) -> Option<Shape> {
    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && args.args.len() == 1
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
        && let Type::Path(inner_path) = inner
        && let Some(inner_last) = inner_path.path.segments.last()
    {
        match inner_last.ident.to_string().as_str() {
            "Value" => return Some(Shape::List),
            "String" => return Some(Shape::ListOf(Box::new(Shape::Str))),
            "BTreeMap" => {
                if map_shape(inner_last).is_some() {
                    return Some(Shape::ListOf(Box::new(Shape::Map)));
                }
            }
            "Vec" => {
                if let Some(nested) = list_shape(inner_last) {
                    return Some(Shape::ListOf(Box::new(nested)));
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolve `BTreeMap<String, Value>` to bare `Map`. Richer value
/// shapes need a `MapOf` tag that does not exist yet; until it does,
/// anything else is a macro error naming the supported spellings.
fn map_shape(segment: &syn::PathSegment) -> Option<Shape> {
    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && args.args.len() == 2
    {
        let mut iter = args.args.iter();
        let (Some(syn::GenericArgument::Type(key)), Some(syn::GenericArgument::Type(value))) =
            (iter.next(), iter.next())
        else {
            return None;
        };
        if let Type::Path(key_path) = key
            && let Some(key_last) = key_path.path.segments.last()
            && key_last.ident == "String"
            && key_last.arguments.is_empty()
            && let Type::Path(value_path) = value
            && let Some(value_last) = value_path.path.segments.last()
            && value_last.ident == "Value"
            && value_last.arguments.is_empty()
        {
            return Some(Shape::Map);
        }
    }
    None
}

/// True when `ty` is written `&mut StepCtx<...>` (any generic arguments).
fn is_step_ctx(ty: &Type) -> bool {
    if let Type::Reference(reference) = ty
        && reference.mutability.is_some()
        && let Type::Path(path) = reference.elem.as_ref()
        && let Some(last) = path.path.segments.last()
    {
        return last.ident == "StepCtx";
    }
    false
}

/// Extract the manager parameter from `&mut StepCtx<M>` (however the path
/// is qualified): the token stream naming `M`, for reuse in the generated
/// registration signature.
fn manager_param_of(ty: &Type) -> syn::Result<TokenStream2> {
    let elem = match ty {
        Type::Reference(reference) => reference.elem.as_ref(),
        other => other,
    };
    if let Type::Path(path) = elem
        && let Some(last) = path.path.segments.last()
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && let Some(syn::GenericArgument::Type(first)) = args.args.first()
    {
        return Ok(quote! { #first });
    }
    Err(syn::Error::new_spanned(
        ty,
        "oxdock_func context must be `&mut StepCtx<P>` with an explicit manager parameter",
    ))
}

/// Collect `///` doc lines in source order.
fn doc_lines(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(pair) = &attr.meta
            && let syn::Expr::Lit(lit) = &pair.value
            && let Lit::Str(text) = &lit.lit
        {
            // Strip only the single space after `///`, preserving any
            // further indentation so fenced examples keep their shape;
            // trailing whitespace is never significant. A full trim
            // here flattened every indented example body.
            let raw = text.value();
            let stripped = raw.strip_prefix(' ').unwrap_or(&raw);
            lines.push(stripped.trim_end().to_string());
        }
    }
    lines
}

/// Map one `#[options(...)]` value type name to its tag expression.
/// Options values are data (scalars and plain collections); handles
/// never hide inside option maps.
fn option_value_type(name: &str) -> syn::Result<TokenStream2> {
    match name {
        "STRING" => Ok(quote! { ::oxdock_core::TypeTag::String }),
        "INT" => Ok(quote! { ::oxdock_core::TypeTag::Int }),
        "FLOAT" => Ok(quote! { ::oxdock_core::TypeTag::Float }),
        "BOOL" => Ok(quote! { ::oxdock_core::TypeTag::Bool }),
        "DURATION" => Ok(quote! { ::oxdock_core::TypeTag::Duration }),
        "PATH" => Ok(quote! { ::oxdock_core::TypeTag::Path }),
        "MAP" => Ok(quote! { ::oxdock_core::TypeTag::Map }),
        "LIST" => Ok(quote! { ::oxdock_core::TypeTag::List }),
        "ANY" => Ok(quote! { ::oxdock_core::TypeTag::Any }),
        _ => Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "unknown options value type; expected STRING, INT, FLOAT, BOOL, DURATION, PATH, MAP, LIST, or ANY",
        )),
    }
}

/// Parse `#[options("key: TYPE", "opt?: TYPE = default", ...)]` into
/// specs. A `?` suffix marks optional keys (defaults need it);
/// anything else fails naming the expected `name[?]: TYPE` shape.
fn parse_options_attr(attr: &syn::Attribute) -> syn::Result<Vec<OptSpec>> {
    let items =
        attr.parse_args_with(syn::punctuated::Punctuated::<LitStr, Token![,]>::parse_terminated)?;
    if items.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[options(...)]` needs at least one entry",
        ));
    }
    let mut specs = Vec::new();
    let mut seen = HashSet::new();
    for lit in items {
        let text = lit.value();
        let spanned = |msg: &str| syn::Error::new(lit.span(), msg);
        let (head, default) = match text.split_once('=') {
            Some((head, default)) => {
                let default = default.trim();
                if default.is_empty() {
                    return Err(spanned("option default must not be empty"));
                }
                (head, Some(default.to_string()))
            }
            None => (text.as_str(), None),
        };
        let (name_part, ty_part) = match head.split_once(':') {
            Some((name, ty)) => (name.trim(), ty.trim()),
            None => {
                return Err(spanned(
                    "expected `name: TYPE` (`?` marks optional keys, `= default` records defaults)",
                ));
            }
        };
        let (name, optional) = match name_part.strip_suffix('?') {
            Some(base) => (base.trim(), true),
            None => (name_part, false),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(spanned("option names are nonempty ASCII identifiers"));
        }
        if default.is_some() && !optional {
            return Err(spanned(
                "option defaults need `?`: required keys have no default",
            ));
        }
        if !seen.insert(name.to_string()) {
            return Err(spanned(&format!("duplicate option `{name}`")));
        }
        let ty = option_value_type(ty_part)
            .map_err(|err| syn::Error::new(lit.span(), err.to_string()))?;
        specs.push(OptSpec {
            name: name.to_string(),
            optional,
            ty,
            default,
        });
    }
    Ok(specs)
}

fn expand_func(options: FuncOptions, func: ItemFn) -> syn::Result<TokenStream2> {
    if func.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            func.sig.fn_token,
            "oxdock_func does not support async functions",
        ));
    }
    if matches!(func.sig.output, syn::ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "oxdock_func functions must return Result<Value>",
        ));
    }

    let ident = func.sig.ident.clone();
    let dsl_name = options
        .name
        .unwrap_or_else(|| ident.to_string().to_uppercase());
    if !is_upper_ident(&dsl_name) {
        return Err(syn::Error::new_spanned(
            &func.sig.ident,
            "oxdock_func name must be an uppercase identifier (ASCII upper, digits, _)",
        ));
    }
    // Registration marker: a unit struct in the type namespace (no collision
    // with the function itself), deriving its name from the Rust identifier.
    // Users group it into a `HostModule` and never name an internal.
    let marker_ident = format_ident!("{}", to_upper_camel_case(&ident.to_string()));
    let marker_docs = format!("Registration marker for `{dsl_name}`: group into a `HostModule`.");
    let (impl_generics, _, _) = func.sig.generics.split_for_impl();

    // Split the context parameter (when present) from DSL parameters.
    // The declaration pattern (`mut cx`, if written) stays on the wrapper
    // signature, but the invocation uses the bare identifier: `mut` in call
    // argument position is invalid Rust.
    let mut inputs = func.sig.inputs.iter();
    let mut cx_pat: Option<TokenStream2> = None;
    let mut cx_ident: Option<syn::Ident> = None;
    let mut cx_ty: Option<TokenStream2> = None;
    let mut manager_param: Option<TokenStream2> = None;
    if !options.pure {
        let first = inputs.next().ok_or_else(|| {
            syn::Error::new_spanned(
                &func.sig,
                "oxdock_func functions take `cx: &mut StepCtx<P>` first (or pass pure)",
            )
        })?;
        if let FnArg::Typed(typed) = first
            && is_step_ctx(&typed.ty)
            && let Pat::Ident(binding) = typed.pat.as_ref()
        {
            let pat = typed.pat.as_ref();
            let ty = typed.ty.as_ref();
            cx_pat = Some(quote! { #pat });
            cx_ident = Some(binding.ident.clone());
            cx_ty = Some(quote! { #ty });
            manager_param = Some(manager_param_of(ty)?);
        } else {
            return Err(syn::Error::new_spanned(
                first,
                "oxdock_func functions take `cx: &mut StepCtx<P>` first as a plain binding (or pass pure)",
            ));
        }
    }

    let mut params = Vec::new();
    for arg in inputs {
        let FnArg::Typed(typed) = arg else {
            return Err(syn::Error::new_spanned(
                arg,
                "oxdock_func does not support receiver arguments",
            ));
        };
        if options.pure && is_step_ctx(&typed.ty) {
            return Err(syn::Error::new_spanned(
                arg,
                "pure oxdock_func functions take no StepCtx; drop pure or drop the context",
            ));
        }
        let Pat::Ident(binding) = typed.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &typed.pat,
                "oxdock_func parameters must be plain bindings",
            ));
        };
        let mut allowed: Option<Vec<String>> = None;
        let mut options: Option<Vec<OptSpec>> = None;
        for attr in &typed.attrs {
            if attr.path().is_ident("values") {
                let parsed = attr.parse_args_with(
                    syn::punctuated::Punctuated::<LitStr, Token![,]>::parse_terminated,
                )?;
                let values: Vec<String> = parsed.into_iter().map(|lit| lit.value()).collect();
                if values.is_empty() {
                    return Err(syn::Error::new_spanned(
                        attr,
                        "`#[values(...)]` needs at least one value",
                    ));
                }
                allowed = Some(values);
            } else if attr.path().is_ident("options") {
                options = Some(parse_options_attr(attr)?);
            }
        }
        let kind = param_kind(&typed.ty)?;
        if allowed.is_some() && !matches!(kind, ParamKind::String) {
            return Err(syn::Error::new_spanned(
                &typed.ty,
                "`#[values(...)]` needs a STRING parameter",
            ));
        }
        if options.is_some() && !matches!(kind, ParamKind::Shaped(Shape::Map)) {
            return Err(syn::Error::new_spanned(
                &typed.ty,
                "`#[options(...)]` needs a MAP parameter",
            ));
        }
        // Per-parameter `///` docs ride the parameter itself: no
        // section to parse, no names to match (position is identity),
        // so prose cannot vary in structure. They pass through to
        // the re-emitted function untouched.
        let param_docs = doc_lines(&typed.attrs).join("\n");
        params.push(Param {
            ident: binding.ident.clone(),
            kind,
            allowed,
            options,
            docs: param_docs,
        });
    }
    // `#[values(...)]`, `#[options(...)]`, and per-parameter `///`
    // docs are macro input, not real attributes: strip them so the
    // re-emitted function compiles (`doc` is not a legal parameter
    // attribute for rustc) without unknown-attribute errors. The prose
    // lives on in the emitted metadata. All other parameter
    // attributes pass through untouched.
    let mut emitted = func.clone();
    for arg in &mut emitted.sig.inputs {
        if let FnArg::Typed(typed) = arg {
            typed.attrs.retain(|attr| {
                !attr.path().is_ident("values")
                    && !attr.path().is_ident("options")
                    && !attr.path().is_ident("doc")
            });
        }
    }

    let arity = params.len();
    let param_names: Vec<&syn::Ident> = params.iter().map(|p| &p.ident).collect();
    let docs = doc_lines(&func.attrs);
    let mut param_metas = Vec::new();
    let mut options_statics = Vec::new();
    // Options statics live inside `registration()`, not at module
    // scope: each function body is its own namespace, so positional
    // names can never collide and no name is ever constructed from
    // function or parameter strings.
    for (option_idx, p) in params.iter().enumerate() {
        let name = p.ident.to_string();
        let kind = p.kind.type_path();
        let param_docs = LitStr::new(&p.docs, proc_macro2::Span::call_site());
        let allowed = match &p.allowed {
            Some(values) => {
                let lits: Vec<LitStr> = values
                    .iter()
                    .map(|value| LitStr::new(value, proc_macro2::Span::call_site()))
                    .collect();
                quote! { Some(&[#(#lits),*]) }
            }
            None => quote! { None },
        };
        let options_meta = match &p.options {
            Some(specs) => {
                let static_ident = format_ident!("__OXDOCK_OPTIONS_{option_idx}");
                let count = specs.len();
                let mut entries = Vec::new();
                for spec in specs {
                    let key = LitStr::new(&spec.name, proc_macro2::Span::call_site());
                    let ty = &spec.ty;
                    let required = !spec.optional;
                    let default = match &spec.default {
                        Some(text) => {
                            let lit = LitStr::new(text, proc_macro2::Span::call_site());
                            quote! { Some(#lit) }
                        }
                        None => quote! { None },
                    };
                    entries.push(quote! {
                        ::oxdock_core::ParamOption {
                            name: #key,
                            value: #ty,
                            required: #required,
                            default: #default,
                        }
                    });
                }
                options_statics.push(quote! {
                    static #static_ident: [::oxdock_core::ParamOption; #count] = [#(#entries),*];
                });
                quote! { Some(&#static_ident) }
            }
            None => quote! { None },
        };
        param_metas.push(quote! {
            ::oxdock_core::FuncParam {
                name: #name.to_string(),
                param_type: #kind,
                allowed: #allowed,
                docs: #param_docs,
                options: #options_meta,
                // The macro has no defaults syntax: generated params
                // are always required. Hand-built entries alone mark
                // trailing options MAPs optional.
                optional: false,
            }
        });
    }
    let unpacks = params.iter().enumerate().map(|(option_idx, p)| {
        let name = &p.ident;
        let extract = p.kind.extractor(name, &dsl_name, p.allowed.as_deref());
        // Options-bearing MAP params validate their keys against the
        // same static the metadata renders: unknown keys fail here,
        // in the wrapper, so bodies never re-check them. The binding
        // is already a map (the extractor enforces MAP), so the check
        // takes it directly with no value round-trip.
        let check = match &p.options {
            Some(_) => {
                let static_ident = format_ident!("__OXDOCK_OPTIONS_{option_idx}");
                quote! {
                    ::oxdock_core::check_options(&#name, &#static_ident, #dsl_name)?;
                }
            }
            None => quote! {},
        };
        quote! {
            let #name = #extract;
            #check
        }
    });
    let values_iter = if params.is_empty() {
        quote! { let _ = __oxdock_values; }
    } else {
        quote! { let mut __oxdock_values = __oxdock_values.into_iter(); }
    };

    let summary = options.summary.unwrap_or_else(|| {
        docs.iter()
            .find(|line| !line.is_empty())
            .cloned()
            .unwrap_or_default()
    });
    let docs_text = docs.join("\n");
    let returns = options
        .returns
        .map(|expr| quote! { Some(#expr) })
        .unwrap_or(quote! { None });

    // The entry point closes over nothing: arity check, unpack, then the
    // original function. A closure keeps it out of the namespace entirely;
    // the original function stays directly callable and unit testable.
    let invoke = match (cx_pat, cx_ident, cx_ty) {
        (Some(pat), Some(cx), Some(ty)) => quote! {
            |#pat: #ty,
             __oxdock_values: Vec<::oxdock_core::Value>|
             -> ::anyhow::Result<::oxdock_core::Value> {
                if __oxdock_values.len() != #arity {
                    ::anyhow::bail!(
                        "{}() expects {} argument(s), got {}",
                        #dsl_name,
                        #arity,
                        __oxdock_values.len(),
                    );
                }
                #values_iter
                #(#unpacks)*
                #ident(#cx, #(#param_names),*)
            }
        },
        _ => quote! {
            |__oxdock_values: Vec<::oxdock_core::Value>|
             -> ::anyhow::Result<::oxdock_core::Value> {
                if __oxdock_values.len() != #arity {
                    ::anyhow::bail!(
                        "{}() expects {} argument(s), got {}",
                        #dsl_name,
                        #arity,
                        __oxdock_values.len(),
                    );
                }
                #values_iter
                #(#unpacks)*
                #ident(#(#param_names),*)
            }
        },
    };

    if options.rpn && options.pure {
        return Err(syn::Error::new_spanned(
            &func.sig.ident,
            "pure oxdock_func functions already run on the math path; drop rpn",
        ));
    }
    let kind = if manager_param.is_some() {
        quote! { ::oxdock_core::FuncKind::HostCtx }
    } else {
        quote! { ::oxdock_core::FuncKind::HostPure }
    };
    let rpn = options.pure || options.rpn;
    let meta = quote! {
        ::oxdock_core::FuncMeta {
            name: #dsl_name.to_string(),
            // Assigned at registration (`register_module` / builtins):
            // markers never know their module.
            module: String::new(),
            kind: #kind,
            params: Some(vec![#(#param_metas),*]),
            returns: #returns,
            rpn: #rpn,
            summary: #summary,
            docs: #docs_text,
        }
    };

    // One `OxDockFn` impl per function. Stateful entries reuse the original
    // generics with the manager bound enforced; pure entries mint a fresh
    // manager parameter (pure functions take no generics of their own,
    // since a generic entry point could never form a function pointer).
    let export = match manager_param {
        Some(manager) => {
            let mut extended = func.sig.generics.clone();
            let bound: syn::WherePredicate =
                syn::parse_quote!(#manager: ::oxdock_core::ProcessManager);
            extended
                .where_clause
                .get_or_insert_with(|| syn::WhereClause {
                    where_token: Default::default(),
                    predicates: Default::default(),
                })
                .predicates
                .push(bound);
            let (_, _, extended_where) = extended.split_for_impl();
            quote! {
                #[doc = #marker_docs]
                pub struct #marker_ident;

                impl #impl_generics ::oxdock_core::OxDockFn<#manager> for #marker_ident
                #extended_where
                {
                    fn registration() -> ::oxdock_core::HostRegistration<#manager> {
                        #(#options_statics)*
                        let func: ::oxdock_core::NativeFn<#manager> =
                            ::std::sync::Arc::new(#invoke);
                        ::oxdock_core::HostRegistration::Stateful {
                            name: #dsl_name.to_string(),
                            meta: #meta,
                            func,
                        }
                    }
                }
            }
        }
        None => {
            if !func.sig.generics.params.is_empty() {
                return Err(syn::Error::new_spanned(
                    &func.sig.generics,
                    "pure oxdock_func functions take no generics; drop pure or drop the type parameters",
                ));
            }
            quote! {
                #[doc = #marker_docs]
                pub struct #marker_ident;

                impl<P: ::oxdock_core::ProcessManager> ::oxdock_core::OxDockFn<P>
                    for #marker_ident
                {
                    fn registration() -> ::oxdock_core::HostRegistration<P> {
                        #(#options_statics)*
                        let func: ::oxdock_core::PureFn =
                            ::std::sync::Arc::new(#invoke);
                        ::oxdock_core::HostRegistration::Pure {
                            name: #dsl_name.to_string(),
                            meta: #meta,
                            func,
                        }
                    }
                }
            }
        }
    };

    Ok(quote! {
        #emitted

        #export
    })
}

/// `#[oxdock_type]` implementation. See the crate docs for the contract.
fn expand_oxdock_type(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream2> {
    let options = syn::parse::<TypeOptions>(attr)?;
    let target = syn::parse::<syn::ItemStruct>(item)?;
    expand_type(options, target)
}

struct TypeOptions {
    name: String,
    summary: Option<String>,
    crate_path: syn::Path,
    inline: bool,
    shared: bool,
}

impl Parse for TypeOptions {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name: Option<String> = None;
        let mut summary: Option<String> = None;
        let mut crate_path: Option<syn::Path> = None;
        let mut inline = false;
        let mut shared = false;
        while !input.is_empty() {
            if input.peek(syn::Ident) && !peek_key_value(input) {
                let flag: syn::Ident = input.parse()?;
                if flag == "inline" {
                    inline = true;
                } else if flag == "shared" {
                    shared = true;
                } else {
                    return Err(syn::Error::new(
                        flag.span(),
                        "unknown oxdock_type flag; expected inline or shared",
                    ));
                }
                if input.peek(Token![,]) {
                    input.parse::<Token![,]>()?;
                }
                continue;
            }
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "crate_path" {
                let value: syn::LitStr = input.parse()?;
                crate_path = Some(value.parse().map_err(|_| {
                    syn::Error::new(
                        key.span(),
                        "oxdock_type crate_path must be a path like \"::oxdock_core\"",
                    )
                })?);
            } else {
                let value: syn::LitStr = input.parse()?;
                if key == "name" {
                    name = Some(value.value());
                } else if key == "summary" {
                    summary = Some(value.value());
                } else {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown oxdock_type option; expected crate_path, name, inline, shared, or summary",
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        let name = name.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "oxdock_type requires name = \"...\" (uppercase descriptor)",
            )
        })?;
        if !is_upper_ident(&name) {
            return Err(syn::Error::new(
                input.span(),
                "oxdock_type name must be an uppercase identifier (ASCII upper, digits, _)",
            ));
        }
        // Generated code paths resolve against this crate root. It defaults
        // to `::oxdock_core` (re-exported there); code expanded inside
        // `oxdock-parser` itself passes `crate_path = "::oxdock_parser"`.
        let default_root: syn::Path = syn::parse_str("::oxdock_core").expect("valid path");
        if inline && shared {
            return Err(syn::Error::new(
                input.span(),
                "oxdock_type cannot be both inline and shared",
            ));
        }
        Ok(TypeOptions {
            name,
            summary,
            crate_path: crate_path.unwrap_or(default_root),
            inline,
            shared,
        })
    }
}

fn is_upper_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => (),
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Convert a `snake_case` Rust identifier to `UpperCamelCase` for the generated
/// registration marker (`make_tag` becomes `MakeTag`).
fn to_upper_camel_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn expand_type(options: TypeOptions, target: syn::ItemStruct) -> syn::Result<TokenStream2> {
    if !target.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &target.generics,
            "oxdock_type markers must not be generic",
        ));
    }
    let struct_ident = target.ident.clone();
    let docs = doc_lines(&target.attrs);
    let summary = options.summary.unwrap_or_else(|| {
        docs.iter()
            .find(|line| !line.is_empty())
            .cloned()
            .unwrap_or_default()
    });
    let docs_text = docs.join("\n");
    let name = options.name;
    let crate_path = options.crate_path;
    // Storage mode selects the vtable: inline payloads copy bytes with no
    // finalizer, exclusive heaps deep-copy and free an owned box, shared
    // heaps bump and release an `Arc` buffer with no copying. All three are
    // monomorphic over the annotated payload struct: no trait objects.
    // `unshare` is the copy-on-write gate used by `Value::read_heap_mut`:
    // exclusive heaps hand out their box, shared heaps detach when the
    // strong count exceeds 1, inline payloads panic (no heap buffer).
    let (clone_hook, drop_hook, eq_hook, fmt_hook, unshare_hook) = if options.inline {
        (
            quote! { #crate_path::clone_copy },
            quote! { #crate_path::drop_noop },
            quote! { #crate_path::eq_inline::<#struct_ident> },
            quote! { #crate_path::fmt_inline::<#struct_ident> },
            quote! { #crate_path::unshare_inline },
        )
    } else if options.shared {
        (
            quote! { #crate_path::clone_shared::<#struct_ident> },
            quote! { #crate_path::drop_shared::<#struct_ident> },
            quote! { #crate_path::eq_shared::<#struct_ident> },
            quote! { #crate_path::fmt_shared::<#struct_ident> },
            quote! { #crate_path::unshare_shared::<#struct_ident> },
        )
    } else {
        (
            quote! { #crate_path::clone_boxed::<#struct_ident> },
            quote! { #crate_path::drop_boxed::<#struct_ident> },
            quote! { #crate_path::eq_boxed::<#struct_ident> },
            quote! { #crate_path::fmt_boxed::<#struct_ident> },
            quote! { #crate_path::unshare_boxed::<#struct_ident> },
        )
    };

    Ok(quote! {
        #target

        impl #crate_path::OxDockType for #struct_ident {
            fn descriptor() -> &'static #crate_path::TypeDescriptor {
                static DESCRIPTOR: #crate_path::TypeDescriptor = #crate_path::TypeDescriptor {
                    name: #name,
                    summary: #summary,
                    docs: #docs_text,
                    clone: #clone_hook,
                    drop: #drop_hook,
                    eq: #eq_hook,
                    fmt: #fmt_hook,
                    unshare: #unshare_hook,
                };
                &DESCRIPTOR
            }
        }
    })
}
