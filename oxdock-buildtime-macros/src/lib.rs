//! Consumer-side macro for assets prepared by `oxdock_buildtime_helpers`.
//!
//! Call [`embed!`] with a single identifier after wiring a build script:
//!
//! ```rust,ignore
//! // build.rs
//! fn main() -> anyhow::Result<()> {
//!     oxdock_buildtime_helpers::embed_assets(
//!         &oxdock_buildtime_helpers::EmbedSpec::new(
//!             "DemoAssets",
//!             oxdock_buildtime_helpers::DslSource::Inline("WRITE hello.txt hi"),
//!         ),
//!     )
//! }
//!
//! // src/main.rs
//! oxdock_buildtime_macros::embed!(DemoAssets);
//! ```
//!
//! The expansion includes the typed module that the helper wrote into
//! `$OUT_DIR`. If build.rs was never wired, cargo fails with a clear
//! "couldn't read …__oxdock_embed_<Name>.rs" error before anything else.

use proc_macro::TokenStream;
use quote::quote;

#[proc_macro]
pub fn embed(input: TokenStream) -> TokenStream {
    let name = syn::parse_macro_input!(input as syn::Ident);
    let file = format!("__oxdock_embed_{name}.rs");
    let file = syn::LitStr::new(&file, name.span());
    let shim = syn::Ident::new(&format!("__oxdock_embed_shim_{name}"), name.span());
    quote! {
        #[allow(non_snake_case)]
        mod #shim {
            include!(concat!(env!("OUT_DIR"), "/", #file));
        }
        pub use #shim::*;
    }
    .into()
}
