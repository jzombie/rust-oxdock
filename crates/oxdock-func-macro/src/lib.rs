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
//!   comes from `returns = "..."` (a descriptor name) and defaults to none.
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
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, ItemFn, Lit, Meta, Pat, Token, Type};

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

#[derive(Default)]
struct FuncOptions {
    pure: bool,
    rpn: bool,
    name: Option<String>,
    returns: Option<String>,
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
                let value: syn::LitStr = input.parse()?;
                if key == "name" {
                    options.name = Some(value.value());
                } else if key == "returns" {
                    options.returns = Some(type_label_to_string(&value)?);
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

fn type_label_to_string(lit: &syn::LitStr) -> syn::Result<String> {
    match lit.value().as_str() {
        "STRING" | "INT" | "FLOAT" | "BOOL" | "PIPE" | "LIST" | "MAP" | "HANDLE" | "DURATION"
        | "PATH" | "SEMAPHORE" | "PERMIT" => Ok(lit.value()),
        other => Err(syn::Error::new(
            lit.span(),
            format!(
                "unknown type label {other:?}; expected a builtin descriptor name such as STRING, INT, LIST, or MAP"
            ),
        )),
    }
}

/// A DSL-facing parameter: binding identifier plus mapped type info.
struct Param {
    ident: syn::Ident,
    kind: ParamKind,
}

#[derive(Clone, Copy)]
enum ParamKind {
    Value,
    String,
    Int,
    Float,
    Bool,
}

impl ParamKind {
    fn type_path(&self) -> TokenStream2 {
        match self {
            ParamKind::Value => quote! { None },
            ParamKind::String => quote! { Some("STRING".to_string()) },
            ParamKind::Int => quote! { Some("INT".to_string()) },
            ParamKind::Float => quote! { Some("FLOAT".to_string()) },
            ParamKind::Bool => quote! { Some("BOOL".to_string()) },
        }
    }

    /// Emitted extractor: turns the next `::oxdock_core::Value` word into
    /// the Rust type, bailing with a `{NAME}()`-prefixed message on
    /// mismatch. Reads go through word accessors; words are never
    /// destructured.
    fn extractor(&self, param: &syn::Ident, dsl_name: &str) -> TokenStream2 {
        match self {
            ParamKind::Value => quote! {
                __oxdock_values.next().expect("arity checked above")
            },
            ParamKind::String => quote! {
                match __oxdock_values.next().expect("arity checked above").as_str() {
                    Some(s) => s.to_string(),
                    None => ::anyhow::bail!(
                        "{}() argument `${}` must be a STRING",
                        #dsl_name,
                        stringify!(#param),
                    ),
                }
            },
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
            _ => {}
        }
    }
    Err(syn::Error::new_spanned(
        ty,
        "unsupported oxdock_func parameter type; expected Value, String, i64, f64, or bool",
    ))
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
        params.push(Param {
            ident: binding.ident.clone(),
            kind: param_kind(&typed.ty)?,
        });
    }

    let arity = params.len();
    let param_names: Vec<&syn::Ident> = params.iter().map(|p| &p.ident).collect();
    let param_metas = params.iter().map(|p| {
        let name = p.ident.to_string();
        let kind = p.kind.type_path();
        quote! {
            ::oxdock_core::FuncParam {
                name: #name.to_string(),
                param_type: #kind,
            }
        }
    });
    let unpacks = params.iter().map(|p| {
        let name = &p.ident;
        let extract = p.kind.extractor(name, &dsl_name);
        quote! { let #name = #extract; }
    });
    let values_iter = if params.is_empty() {
        quote! { let _ = __oxdock_values; }
    } else {
        quote! { let mut __oxdock_values = __oxdock_values.into_iter(); }
    };

    let docs = doc_lines(&func.attrs);
    let summary = options.summary.unwrap_or_else(|| {
        docs.iter()
            .find(|line| !line.is_empty())
            .cloned()
            .unwrap_or_default()
    });
    let docs_text = docs.join("\n");
    let returns = options
        .returns
        .map(|label| quote! { Some(#label.to_string()) })
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
        #func

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
