//! Shared filename contract between the build-script helper and the
//! consumer-side `embed!` macro.

/// The `$OUT_DIR`-relative file name of the generated module for `name`.
pub fn module_file_name(name: &str) -> String {
    format!("__oxdock_embed_{name}.rs")
}

#[cfg(test)]
mod tests {
    use super::module_file_name;

    #[test]
    fn embed_module_ident_prefixes_struct_name() {
        assert_eq!(
            module_file_name("DemoAssets"),
            "__oxdock_embed_DemoAssets.rs"
        );
    }
}
