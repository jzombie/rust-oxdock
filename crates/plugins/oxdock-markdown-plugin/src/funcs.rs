//! The `MARKDOWN` host module: record data to Markdown table (issue 170).
//!
//! Thin OxDock wrapper over [`crate::markdown`], the single source for
//! table rendering. Values pass straight through on the shared value
//! model with no conversion.

use anyhow::Result;
use oxdock_core::{HostModule, OxDockFn, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::ProcessManager;

/// Render record data as an aligned Markdown table: one row per MAP,
/// one column per key. Fails strict on shapes with no table in them.
#[oxdock_func(pure, returns = TypeTag::String)]
fn map_to_md_table(map: Value) -> Result<Value> {
    let table = crate::markdown::map_to_table(&map)?;
    Ok(Value::string(table))
}

/// The `MARKDOWN` host module for OxDock scripts and placeholders.
pub fn module_with<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "MARKDOWN".to_string(),
        funcs: vec![MapToMdTable::registration()],
        types: vec![],
        record_schemas: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn renders_map_as_table() {
        let value = map(&[
            ("name", Value::string("demo".to_string())),
            ("stars", Value::int(3)),
        ]);
        let out = map_to_md_table(value)
            .expect("table")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(
            out,
            "| name | stars |\n| ---- | ----- |\n| demo | 3     |\n"
        );
    }

    #[test]
    fn rejects_non_maps() {
        assert!(map_to_md_table(Value::string("nope".to_string())).is_err());
    }
}
