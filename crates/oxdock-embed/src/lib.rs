use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

pub fn runtime_support_tokens() -> TokenStream {
    quote! {
        extern crate alloc;

        use alloc::borrow::Cow;

        #[derive(Clone)]
        pub struct Metadata {
            hash: [u8; 32],
            last_modified: Option<u64>,
            created: Option<u64>,
        }

        impl Metadata {
            pub const fn __oxdock_new(
                hash: [u8; 32],
                last_modified: Option<u64>,
                created: Option<u64>,
            ) -> Self {
                Self {
                    hash,
                    last_modified,
                    created,
                }
            }

            pub fn sha256_hash(&self) -> [u8; 32] {
                self.hash
            }

            pub fn last_modified(&self) -> Option<u64> {
                self.last_modified
            }

            pub fn created(&self) -> Option<u64> {
                self.created
            }
        }

        #[derive(Clone)]
        pub struct EmbeddedFile {
            pub data: Cow<'static, [u8]>,
            pub metadata: Metadata,
        }

        pub enum Filenames {
            Embedded(core::slice::Iter<'static, &'static str>),
        }

        impl Filenames {
            pub fn from_slice(slice: &'static [&'static str]) -> Self {
                Self::Embedded(slice.iter())
            }
        }

        impl Iterator for Filenames {
            type Item = Cow<'static, str>;

            fn next(&mut self) -> Option<Self::Item> {
                match self {
                    Filenames::Embedded(iter) => iter.next().map(|s| Cow::Borrowed(*s)),
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AssetRecord {
    pub rel_path: String,
    pub include_path: String,
    pub sha256: [u8; 32],
    pub last_modified: Option<u64>,
    pub created: Option<u64>,
}

pub fn gather_assets(
    resolver: &PathResolver,
    asset_root: &GuardedPath,
) -> Result<Vec<AssetRecord>> {
    let mut assets = Vec::new();
    collect_assets_recursive(resolver, asset_root, asset_root, &mut assets)?;
    assets.sort_by_key(|asset| asset.rel_path.clone());
    Ok(assets)
}

fn collect_assets_recursive(
    resolver: &PathResolver,
    root: &GuardedPath,
    dir: &GuardedPath,
    assets: &mut Vec<AssetRecord>,
) -> Result<()> {
    let mut entries = resolver
        .read_dir_entries(dir)
        .with_context(|| format!("failed to read assets in {}", dir.as_path().display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let entry_path = entry.path();
        let entry_guard = GuardedPath::new(root.root(), entry_path.as_path())
            .with_context(|| format!("failed to guard entry {}", entry_path.as_path().display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to read file type for {}",
                entry_guard.as_path().display()
            )
        })?;

        if file_type.is_dir() {
            collect_assets_recursive(resolver, root, &entry_guard, assets)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let rel_path = entry_guard
            .as_path()
            .strip_prefix(root.as_path())
            .with_context(|| {
                format!(
                    "embedded file {} not under {}",
                    entry_guard.as_path().display(),
                    root.as_path().display()
                )
            })?;
        let rel_str = rel_path.to_str().with_context(|| {
            format!(
                "embedded file path is not valid UTF-8: {}",
                entry_guard.as_path().display()
            )
        })?;
        let rel_forward = oxdock_fs::to_forward_slashes(rel_str);
        let include_path = oxdock_fs::normalized_path(&entry_guard);

        let bytes = resolver
            .read_file(&entry_guard)
            .with_context(|| format!("failed to read {} for hashing", rel_forward))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha256: [u8; 32] = hasher.finalize().into();

        let metadata = resolver
            .metadata(&entry_guard)
            .with_context(|| format!("failed to read metadata for {}", rel_forward))?;
        let last_modified = metadata
            .modified()
            .ok()
            .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let created = metadata
            .created()
            .ok()
            .and_then(|ctime| ctime.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        assets.push(AssetRecord {
            rel_path: rel_forward,
            include_path,
            sha256,
            last_modified,
            created,
        });
    }

    Ok(())
}

pub fn emit_embed_module(name: &syn::Ident, assets: &[AssetRecord]) -> syn::Result<TokenStream> {
    let mod_ident = format_ident!("__oxdock_embed_{}", name);
    let runtime_support = runtime_support_tokens();
    let bytes_idents: Vec<_> = (0..assets.len())
        .map(|idx| format_ident!("__OXDOCK_EMBED_BYTES_{idx}"))
        .collect();
    let abs_paths: Vec<_> = assets
        .iter()
        .map(|asset| syn::LitStr::new(&asset.include_path, proc_macro2::Span::call_site()))
        .collect();
    let rel_paths: Vec<_> = assets
        .iter()
        .map(|asset| syn::LitStr::new(&asset.rel_path, proc_macro2::Span::call_site()))
        .collect();

    let bytes_consts = bytes_idents
        .iter()
        .zip(abs_paths.iter())
        .map(|(ident, abs)| quote! { const #ident: &[u8] = include_bytes!(#abs); });

    let metadata_tokens: Vec<_> = assets
        .iter()
        .map(|asset| {
            let hash_bytes: Vec<_> = asset
                .sha256
                .iter()
                .map(|b| {
                    let lit = proc_macro2::Literal::u8_unsuffixed(*b);
                    quote! { #lit }
                })
                .collect();
            let last_modified = match asset.last_modified {
                Some(v) => {
                    let lit = syn::LitInt::new(&v.to_string(), proc_macro2::Span::call_site());
                    quote! { Some(#lit) }
                }
                None => quote! { None },
            };
            let created = match asset.created {
                Some(v) => {
                    let lit = syn::LitInt::new(&v.to_string(), proc_macro2::Span::call_site());
                    quote! { Some(#lit) }
                }
                None => quote! { None },
            };
            quote! {
                Metadata::__oxdock_new(
                    [#(#hash_bytes),*],
                    #last_modified,
                    #created
                )
            }
        })
        .collect();

    let asset_entries: Vec<_> = rel_paths
        .iter()
        .zip(bytes_idents.iter())
        .zip(metadata_tokens.iter())
        .map(|((rel, ident), metadata)| {
            quote! {
                AssetEntry {
                    rel: #rel,
                    data: #ident,
                    metadata: #metadata,
                }
            }
        })
        .collect();

    Ok(quote! {
        #[allow(clippy::disallowed_methods, clippy::disallowed_types, non_snake_case)]
        pub mod #mod_ident {
            #runtime_support

            #[derive(Clone)]
            struct AssetEntry {
                rel: &'static str,
                data: &'static [u8],
                metadata: Metadata,
            }

            #( #bytes_consts )*
            const __OXDOCK_EMBED_FILENAMES: &[&str] = &[
                #(#rel_paths),*
            ];
            const __OXDOCK_EMBED_ASSETS: &[AssetEntry] = &[
                #(#asset_entries),*
            ];

            pub struct #name;

            impl #name {
                pub fn get(path: &str) -> Option<EmbeddedFile> {
                    __OXDOCK_EMBED_ASSETS
                        .iter()
                        .find(|entry| entry.rel == path)
                        .map(|entry| EmbeddedFile {
                            data: Cow::Borrowed(entry.data),
                            metadata: entry.metadata.clone(),
                        })
                }

                pub fn iter() -> Filenames {
                    Filenames::from_slice(__OXDOCK_EMBED_FILENAMES)
                }
            }
        }

            pub use #mod_ident::#name;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::format_ident;

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `TokenStream::to_string` inserts spaces between idents and punctuation
    /// (`include_bytes !`, `Some (123)`); squash them for substring checks.
    fn squashed(tokens: &TokenStream) -> String {
        tokens.to_string().replace(' ', "")
    }

    fn write_asset(root: &GuardedPath, resolver: &PathResolver, rel: &str, body: &[u8]) {
        let path = root.join(rel).expect("join asset rel");
        resolver.ensure_parent_dir(&path).expect("ensure parent");
        resolver.write_file(&path, body).expect("write asset");
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir uses real filesystem ops blocked by Miri isolation"
    )]
    #[test]
    fn gather_assets_walks_nested_tree_sorted_with_forward_slashes() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");

        let assets_dir = root.join("assets").expect("assets dir");
        resolver.create_dir_all(&assets_dir).expect("mkdir");
        write_asset(&assets_dir, &resolver, "b.txt", b"b");
        write_asset(&assets_dir, &resolver, "sub/a.txt", b"a");
        write_asset(&assets_dir, &resolver, "sub/deep/c.txt", b"c");

        let assets = gather_assets(&resolver, &assets_dir).expect("gather");
        let rels: Vec<&str> = assets.iter().map(|a| a.rel_path.as_str()).collect();
        assert_eq!(rels, vec!["b.txt", "sub/a.txt", "sub/deep/c.txt"]);
        for asset in &assets {
            assert!(
                !asset.include_path.contains('\\'),
                "include paths must be normalized: {}",
                asset.include_path
            );
        }
    }

    #[cfg_attr(
        miri,
        ignore = "GuardedPath::tempdir uses real filesystem ops blocked by Miri isolation"
    )]
    #[test]
    fn gather_assets_hashes_content_sha256_known_digest() {
        // sha256("hello")
        const HELLO_DIGEST: &str =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        let assets_dir = root.join("assets").expect("assets dir");
        resolver.create_dir_all(&assets_dir).expect("mkdir");
        write_asset(&assets_dir, &resolver, "hello.bin", b"hello");

        let assets = gather_assets(&resolver, &assets_dir).expect("gather");
        assert_eq!(
            assets.len(),
            1,
            "control files must live outside asset roots"
        );
        assert_eq!(hex_encode(&assets[0].sha256), HELLO_DIGEST);
    }

    fn sample_asset(rel: &str, include: &str, hash_first_byte: u8) -> AssetRecord {
        let mut sha256 = [0u8; 32];
        sha256[0] = hash_first_byte;
        AssetRecord {
            rel_path: rel.to_string(),
            include_path: include.to_string(),
            sha256,
            last_modified: Some(123),
            created: None,
        }
    }

    #[test]
    fn emit_embed_module_produces_parseable_module_with_assets() {
        let assets = vec![
            sample_asset("data/a.txt", "/abs/workspace/data/a.txt", 0x2c),
            sample_asset("data/b.txt", "/abs/workspace/data/b.txt", 0xf2),
        ];
        let name = format_ident!("Assets");
        let tokens = emit_embed_module(&name, &assets).expect("emit");

        // The generated module must be syntactically valid Rust.
        syn::parse2::<syn::File>(tokens.clone()).expect("generated tokens must parse");

        let rendered = squashed(&tokens);
        assert_eq!(rendered.matches("include_bytes!(").count(), assets.len());
        assert!(rendered.contains("\"/abs/workspace/data/a.txt\""));
        assert!(rendered.contains("\"/abs/workspace/data/b.txt\""));
        // Relative paths appear both as lookup keys and in the filenames table.
        assert_eq!(rendered.matches("\"data/a.txt\"").count(), 2);
        assert!(rendered.contains("__oxdock_embed_Assets"));
    }

    #[test]
    fn emit_embed_module_handles_empty_asset_list() {
        let name = format_ident!("Empty");
        let tokens = emit_embed_module(&name, &[]).expect("emit");
        syn::parse2::<syn::File>(tokens.clone()).expect("empty emission must parse");

        let rendered = squashed(&tokens);
        assert!(!rendered.contains("include_bytes!("));
        assert!(rendered.contains("__oxdock_embed_Empty"));
    }

    #[test]
    fn emit_embed_module_renders_timestamp_variants() {
        let present = sample_asset("with-time.txt", "/abs/with-time.txt", 1);
        let absent = AssetRecord {
            last_modified: None,
            created: None,
            ..sample_asset("no-time.txt", "/abs/no-time.txt", 2)
        };
        let tokens = emit_embed_module(&format_ident!("Mixed"), &[present, absent]).expect("emit");

        let rendered = squashed(&tokens);
        assert!(rendered.contains("Some(123)"), "timestamps render as Some");
        assert!(
            rendered.contains("None"),
            "missing timestamps render as None"
        );
    }

    #[test]
    fn runtime_support_tokens_are_valid_and_expose_metadata_api() {
        let tokens = runtime_support_tokens();
        syn::parse2::<syn::File>(tokens.clone()).expect("runtime support must parse");

        let rendered = tokens.to_string();
        assert!(rendered.contains("pub fn sha256_hash"));
        assert!(rendered.contains("pub struct EmbeddedFile"));
        assert!(rendered.contains("impl Iterator for Filenames"));
    }
}
