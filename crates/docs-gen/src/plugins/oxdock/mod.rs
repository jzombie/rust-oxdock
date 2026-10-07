//! Internal `OXDOCK` plugin: registry introspection for pipelines.
//!
//! Command index and body plus function and plugin type references,
//! all rendered live from parser and registry metadata so docs can
//! never list a removed command. Rendering lives in `command_ref.rs`,
//! the single source the host wrappers call into. Registered only by
//! `crate::run`; reusable by any OxDock-based project documenting
//! itself, registered by no project outside OxDock.

mod command_ref;

use anyhow::Result;
use oxdock_core::{FuncMeta, HostModule, OxDockFn, TypeDescriptor, TypeTag, Value};
use oxdock_func_macro::oxdock_func;
use oxdock_process::{DefaultProcessManager, ProcessManager};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Generated command index table from parser metadata, so docs can never
/// list a removed command.
#[oxdock_func(pure, returns = TypeTag::String)]
fn command_index() -> Result<Value> {
    Ok(Value::string(command_ref::render_index()))
}

/// Generated command body from parser metadata.
#[oxdock_func(pure, returns = TypeTag::String)]
fn command_body() -> Result<Value> {
    Ok(Value::string(command_ref::render_body()?))
}

/// Generated value type reference from the startup descriptors.
/// Rendered separately from the command body so templates place the
/// Value types section independently of the command details.
#[oxdock_func(pure, returns = TypeTag::String)]
fn value_types() -> Result<Value> {
    Ok(Value::string(command_ref::render_value_types()))
}

/// Generated function reference from the `#[oxdock_func]` registry.
#[oxdock_func(pure, returns = TypeTag::String)]
fn function_reference() -> Result<Value> {
    Ok(Value::string(command_ref::render_function_reference()))
}

/// Generated function reference for one plugin module.
#[oxdock_func(pure, returns = TypeTag::String)]
fn plugin_function_reference(module: String) -> Result<Value> {
    let entry = plugin_entry(&module)?;
    Ok(Value::string(command_ref::render_plugin_reference(
        &entry.metas,
        &module,
    )))
}

/// Generated value-type reference for one plugin module's handle types.
#[oxdock_func(pure, returns = TypeTag::String)]
fn plugin_type_reference(module: String) -> Result<Value> {
    let entry = plugin_entry(&module)?;
    Ok(Value::string(command_ref::render_plugin_types(
        &entry.types,
        &entry.metas,
    )))
}

/// Render one generated input by config key, so the pipeline loops
/// over `generated` entries instead of hardcoding one call per
/// artifact. The key-to-call mapping lives here, with the OxDock
/// domain, not in the generic engine. Unknown keys fail listing the
/// known ones, the same strict UX as unknown placeholder calls.
#[oxdock_func(pure, returns = TypeTag::String)]
fn generated(key: String) -> Result<Value> {
    match key.as_str() {
        "command_index" => command_index(),
        "command_body" => command_body(),
        "value_types" => value_types(),
        "function_reference" => function_reference(),
        "ssh_function_reference" => plugin_function_reference("SSH".to_string()),
        "ssh_type_reference" => plugin_type_reference("SSH".to_string()),
        "net_function_reference" => plugin_function_reference("NET".to_string()),
        "net_type_reference" => plugin_type_reference("NET".to_string()),
        "markdown_function_reference" => plugin_function_reference("MARKDOWN".to_string()),
        _ => {
            const KNOWN: &[&str] = &[
                "command_index",
                "command_body",
                "value_types",
                "function_reference",
                "ssh_function_reference",
                "ssh_type_reference",
                "net_function_reference",
                "net_type_reference",
                "markdown_function_reference",
            ];
            anyhow::bail!(
                "unknown generated key '{key}'; known keys: {}",
                KNOWN.join(", ")
            )
        }
    }
}

/// The internal `OXDOCK` plugin for the render engine. Registered only
/// by `crate::run`; the core language never sees it.
pub fn module<P: ProcessManager>() -> HostModule<P> {
    HostModule {
        name: "OXDOCK".to_string(),
        funcs: vec![
            CommandIndex::registration(),
            CommandBody::registration(),
            ValueTypes::registration(),
            FunctionReference::registration(),
            PluginFunctionReference::registration(),
            PluginTypeReference::registration(),
            Generated::registration(),
        ],
        types: vec![],
        record_schemas: vec![],
    }
}

/// Documented plugin modules, read live from each plugin's `HostModule`
/// (the single source scripts register) instead of a parallel registry.
/// Construction binds nothing: both modules build over a fresh virtual
/// endpoint registry with no sockets.
struct PluginDocs {
    metas: Vec<FuncMeta>,
    types: Vec<&'static TypeDescriptor>,
}

fn plugin_docs() -> &'static HashMap<String, PluginDocs> {
    static DOCS: OnceLock<HashMap<String, PluginDocs>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let mut map = HashMap::new();
        let modules: Vec<HostModule<DefaultProcessManager>> = vec![
            oxdock_ssh_plugin::module(),
            oxdock_net_plugin::module(),
            oxdock_markdown_plugin::module(),
        ];
        for module in modules {
            let name = module.name.clone();
            let mut metas: Vec<FuncMeta> = module
                .funcs
                .iter()
                .map(|registration| {
                    let mut meta = registration.meta().clone();
                    // Raw metas leave `module` empty for registration-time
                    // stamping; stamp it here from the owning module.
                    meta.module = name.clone();
                    meta
                })
                .collect();
            metas.sort_by(|left, right| left.name.cmp(&right.name));
            map.insert(
                name,
                PluginDocs {
                    metas,
                    types: module.types,
                },
            );
        }
        map
    })
}

/// Look up one plugin's docs by module name, bailing with the known
/// names so a renamed module fails the run instead of rendering empty.
fn plugin_entry(module: &str) -> Result<&'static PluginDocs> {
    let docs = plugin_docs();
    docs.get(module).ok_or_else(|| {
        let mut known: Vec<&str> = docs.keys().map(String::as_str).collect();
        known.sort_unstable();
        anyhow::anyhow!(
            "unknown plugin module '{module}'; known modules: {}",
            known.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_reference_covers_each_module() {
        let ssh = plugin_function_reference("SSH".to_string())
            .expect("ssh reference")
            .as_str()
            .expect("string")
            .to_string();
        for name in [
            "SSH_SERVE",
            "SSH_ACCEPT",
            "SSH_DEQUEUE",
            "SSH_PUMP_CHANNEL",
            "SSH_CLOSE",
            "SSH_CONNECT",
            "SSH_PUMP",
            "SSH_PTY_RUN",
        ] {
            assert!(
                ssh.contains(&format!("### {name}")),
                "SSH reference must document {name}",
            );
        }
        assert!(
            !ssh.contains("STD::GLOB"),
            "SSH reference must not contain STD entries",
        );
        assert!(
            !ssh.contains("NET_LISTEN"),
            "SSH reference must not contain NET entries",
        );
        let net = plugin_function_reference("NET".to_string())
            .expect("net reference")
            .as_str()
            .expect("string")
            .to_string();
        for name in ["NET_LISTEN", "NET_ACCEPT", "NET_CLOSE", "NET_CONNECT"] {
            assert!(
                net.contains(&format!("### {name}")),
                "NET reference must document {name}",
            );
        }
        assert!(
            !net.contains("SSH_SERVE"),
            "NET reference must not contain SSH entries",
        );
        let markdown = plugin_function_reference("MARKDOWN".to_string())
            .expect("markdown reference")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            markdown.contains("### MAP_TO_MD_TABLE"),
            "MARKDOWN reference must document MAP_TO_MD_TABLE",
        );
        assert!(
            !markdown.contains("NET_FETCH"),
            "MARKDOWN reference must not contain NET entries",
        );
    }

    #[test]
    fn plugin_reference_rejects_unknown_modules() {
        let err = plugin_function_reference("NOPE".to_string()).expect_err("unknown must fail");
        let text = format!("{err:#}");
        assert!(text.contains("NOPE"), "error names the module: {text}");
        assert!(text.contains("SSH"), "error lists SSH: {text}");
        assert!(text.contains("NET"), "error lists NET: {text}");
    }

    #[test]
    fn plugin_type_reference_names_handle_types() {
        let ssh = plugin_type_reference("SSH".to_string())
            .expect("ssh types")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            ssh.contains("SSH_SERVER"),
            "SSH types name the server: {ssh}"
        );
        assert!(
            ssh.contains("SSH_SESSION"),
            "SSH types name the session: {ssh}"
        );
        // Origins derive from metadata, never prose: every handle names
        // its minter and its users exactly, so a new consumer fails
        // loudly instead of rotting a hand list.
        for (name, line) in [
            (
                "SSH_SERVER",
                "Minted by `SSH_SERVE`; used by `SSH_ACCEPT`, `SSH_CLOSE`, `SSH_DEQUEUE`.",
            ),
            (
                "SSH_SESSION",
                "Minted by `SSH_DEQUEUE`; used by `SSH_PTY_RUN`, `SSH_PUMP_CHANNEL`.",
            ),
        ] {
            let section = ssh
                .split(&format!("### Value type: {name}"))
                .nth(1)
                .unwrap_or("");
            assert!(section.contains(line), "{name} pins its origins: {section}");
        }
        let net = plugin_type_reference("NET".to_string())
            .expect("net types")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            net.contains("NET_LISTENER"),
            "NET types name the listener: {net}"
        );
        let section = net
            .split("### Value type: NET_LISTENER")
            .nth(1)
            .unwrap_or("");
        assert!(
            section.contains("Minted by `NET_LISTEN`; used by `NET_ACCEPT`, `NET_CLOSE`."),
            "listener pins its origins: {section}"
        );
    }

    #[test]
    fn plugin_returns_stay_within_known_types() {
        // Every `returns` tag must resolve against startup types plus the
        // module's own descriptors. Tags make typos unspellable for
        // builtins; this test pins the custom and shaped remainder, and
        // run-start validation enforces the same rule for every run.
        use oxdock_core::TypeTag;
        fn leaves(tag: &TypeTag, out: &mut Vec<&'static str>) {
            match tag {
                TypeTag::Custom(descriptor) => out.push(descriptor.name),
                TypeTag::ListOf(element) => leaves(element, out),
                TypeTag::Record(fields) => {
                    for field in *fields {
                        leaves(&field.ty, out);
                    }
                }
                _ => {}
            }
        }
        let startup: std::collections::HashSet<&str> = oxdock_core::startup_descriptors()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for (module, entry) in plugin_docs() {
            let mut known = startup.clone();
            for descriptor in &entry.types {
                known.insert(descriptor.name);
            }
            for meta in &entry.metas {
                if let Some(returns) = meta.returns.as_ref() {
                    let mut pending = Vec::new();
                    leaves(returns, &mut pending);
                    for name in pending {
                        assert!(
                            known.contains(name),
                            "{module}::{} returns unregistered type '{name}'",
                            meta.name,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn generated_dispatch_renders_known_keys() {
        let index = generated("command_index".to_string())
            .expect("index")
            .as_str()
            .expect("string")
            .to_string();
        assert!(index.contains("## Command Reference"), "got: {index}");
        let types = generated("value_types".to_string())
            .expect("value types")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            types.contains("## Value types"),
            "value types split renders its own section: {types}"
        );
        let body = generated("command_body".to_string())
            .expect("body")
            .as_str()
            .expect("string")
            .to_string();
        assert!(
            !body.contains("## Value types"),
            "command body no longer carries the value types tail"
        );
    }

    #[test]
    fn generated_dispatch_rejects_unknown_keys() {
        let err = generated("nope".to_string()).expect_err("unknown must fail");
        let text = format!("{err:#}");
        assert!(text.contains("nope"), "error names the key: {text}");
        assert!(text.contains("command_index"), "error lists keys: {text}");
    }

    #[test]
    fn renderers_stay_non_empty() {
        assert!(
            command_index()
                .expect("index")
                .as_str()
                .expect("string")
                .contains("## Command Reference")
        );
        assert!(
            function_reference()
                .expect("functions")
                .as_str()
                .expect("string")
                .contains("## Functions")
        );
    }
}
