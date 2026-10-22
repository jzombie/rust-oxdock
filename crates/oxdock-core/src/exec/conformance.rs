//! Returns-slot conformance runner: executes every function in a
//! [`HostModule`] against its declared `returns` slot, so generated
//! docs (rendered from live metadata) cannot lie or rot.
//!
//! Two execution paths feed one `Ok` counter per function. Sampled
//! direct invocations cover what blind deterministic arguments can
//! reach; executable `Example` scripts cover the rest (domain
//! preconditions, sessions, listeners) by running end to end with the
//! module under test on a tempdir filesystem. Both run through the normal
//! host-call paths, so the runtime slot check enforces returns on
//! every `Ok`. Error-demonstrating examples document failure modes
//! but cover nothing: only completed `Ok` executions count. Rendered
//! fences execute once more under docs_conformance with the real
//! harnesses; the two executions agree on hermetic examples.
//!
//! Everything runs on watchdog threads with a deadline exceeding
//! every legitimate sampled timeout. Pumps on live pipes and dials
//! to blackholes block indefinitely; a stuck sample reports as
//! inconclusive instead of hanging the suite, and docs_conformance
//! owns the hang verdict for example scripts.
//!
//! Compiled under `cfg(test)` and the `test-harness` feature alike:
//! core's own suite uses the former, downstream integration suites
//! enable the feature in dev-dependencies. Mock filesystem
//! boundaries come from the feature-gated mocks, so sampled calls
//! run with zero I/O outside the mock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use oxdock_fs::{GuardedPath, MockFs, PathResolver, WorkspaceFs};
use oxdock_parser::{ModuleFuncs, ModuleTable};

use super::args::check_host_return_value;
use super::base_name;
use super::io::ExecIo;
use super::native::{FuncParam, HostModule, HostRegistration, builtin_function_metas};
use super::state::ExecState;
use super::steps::StepCtx;
use super::typing::{TypeTag, Value};
use oxdock_process::{CommandStdin, ProcessManager};

/// Watchdog deadline per sampled call and per example script: exceeds
/// every legitimate sampled timeout (dial and request deadlines) with
/// margin, so only true blockers trip it.
const WATCHDOG_SECS: u64 = 60;

/// Assert every function in `funcs` honors its declared `returns`.
/// `module_name` qualifies names and keys the parse table; `funcs`
/// supply sampling bodies and example entries; `exec_modules` are the
/// modules registered for example runs (usually the module `funcs`
/// came from; empty for STD, whose builtins the run pre-registers).
/// Takes process by value: invocations run on watchdog threads, so
/// everything crossing the boundary must be owned. Panics naming
/// every offending function at once; `Err` invocations and expired
/// deadlines are inconclusive and never count for or against coverage.
pub fn assert_module_returns_conformance<P: ProcessManager + Clone + Send + 'static>(
    module_name: &str,
    funcs: &[HostRegistration<P>],
    exec_modules: Vec<HostModule<P>>,
    process: P,
) {
    // Accumulate every failure and report once: a single run names the
    // whole uncovered set instead of stopping at the first offender.
    let mut failures: Vec<String> = Vec::new();
    for registration in funcs {
        let (base, meta) = match registration {
            HostRegistration::Pure { name, meta, .. } => (name.clone(), meta),
            HostRegistration::Stateful { name, meta, .. } => (name.clone(), meta),
            HostRegistration::Both { name, meta, .. } => (name.clone(), meta),
        };
        let qualified = format!("{module_name}::{base}");
        if meta.returns.is_none() {
            continue;
        }
        // Building vectors runs the options-count assertion even when
        // invocation is skipped below, so cross-products cannot creep
        // back unnoticed for example-covered functions.
        let vectors = sample_vectors(meta);
        let mut ok = 0;
        // Examples displace sampling: their executions verify the slot
        // on representative paths, while blind sampling a function
        // whose preconditions need staging (pumps on live pipes, dials)
        // would burn the watchdog on every suite run. Error-demonstrating
        // examples document failure but cover nothing: only `Passed`
        // counts toward the `Ok` requirement below.
        if meta.examples.is_empty() {
            for args in vectors {
                match invoke_sample(registration, args, process.clone()) {
                    SampleOutcome::Ok(value) => {
                        if let Err(err) = check_host_return_value(
                            &format!("{qualified} returns"),
                            &meta.returns,
                            &value,
                        ) {
                            failures.push(format!("{qualified}: lying returns: {err:#}"));
                        } else {
                            ok += 1;
                        }
                    }
                    SampleOutcome::Err | SampleOutcome::TimedOut => {}
                }
            }
        }
        for example in meta.examples {
            if !mentions(example.code, &base) {
                failures.push(format!(
                    "{qualified} example '{}' never names `{base}`",
                    example.name
                ));
                continue;
            }
            match run_example(module_name, funcs, &exec_modules, example, process.clone()) {
                ExampleOutcome::Passed => ok += 1,
                ExampleOutcome::ExpectedError => {}
                ExampleOutcome::Failed(reason) => {
                    failures.push(format!("{qualified} example '{}': {reason}", example.name))
                }
                ExampleOutcome::TimedOut => {}
            }
        }
        if ok == 0 {
            failures.push(format!(
                "{qualified}: no sampled call and no example reached Ok; add passing coverage or widen the sampler"
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "returns conformance failed ({}):\n- {}",
            failures.len(),
            failures.join("\n- ")
        );
    }
}

/// One sampled direct invocation.
enum SampleOutcome {
    Ok(Value),
    Err,
    TimedOut,
}

/// Invoke one sampled argument vector on a watchdog thread with fresh
/// mock state: pipe samples are take-once, so shared backends would
/// poison later vectors, and everything the thread touches is owned.
fn invoke_sample<P: ProcessManager + Send + 'static>(
    registration: &HostRegistration<P>,
    args: Vec<Value>,
    process: P,
) -> SampleOutcome {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // Detached watchdog threads: dropping the join handle detaches,
    // so a stuck sample outlives the suite instead of hanging it.
    // Bare `spawn` statements state that intent.
    match registration {
        HostRegistration::Pure { func, .. } => {
            let func = func.clone();
            std::thread::spawn(move || {
                let _ = done_tx.send(match func(args) {
                    Ok(value) => SampleOutcome::Ok(value),
                    Err(_) => SampleOutcome::Err,
                });
            });
        }
        // Dual entries sample through the template arm: it answers
        // strings without staging pipes, while pipe samples bail fast
        // instead of blocking on backends no producer feeds.
        HostRegistration::Both { template, .. } => {
            let func = template.clone();
            std::thread::spawn(move || {
                let _ = done_tx.send(match func(args) {
                    Ok(value) => SampleOutcome::Ok(value),
                    Err(_) => SampleOutcome::Err,
                });
            });
        }
        HostRegistration::Stateful { func, .. } => {
            let func = func.clone();
            std::thread::spawn(move || {
                let mut state: ExecState<P> = mock_state();
                let mut process = process;
                let mut cx = StepCtx {
                    state: &mut state,
                    process: &mut process,
                    stdin: CommandStdin::Null,
                    expose_stdin: false,
                    out: None,
                    err: None,
                    out_pipe: None,
                    stdin_pipe: None,
                };
                let _ = done_tx.send(match func(&mut cx, args) {
                    Ok(value) => SampleOutcome::Ok(value),
                    Err(_) => SampleOutcome::Err,
                });
            });
        }
    }
    match done_rx.recv_timeout(std::time::Duration::from_secs(WATCHDOG_SECS)) {
        Ok(outcome) => outcome,
        Err(_) => SampleOutcome::TimedOut,
    }
}

/// One example script execution.
enum ExampleOutcome {
    Passed,
    ExpectedError,
    Failed(String),
    TimedOut,
}

/// Execute one example end to end with `exec_modules` registered, on
/// a mock filesystem with the caller's process manager: the identical
/// shape to a docs_conformance fence run, so the runtime slot check
/// enforces returns on this path too.
fn run_example<P: ProcessManager + Clone + Send + 'static>(
    module_name: &str,
    funcs: &[HostRegistration<P>],
    exec_modules: &[HostModule<P>],
    example: &oxdock_parser::Example,
    process: P,
) -> ExampleOutcome {
    let mut table = ModuleTable {
        modules: HashMap::from([(
            "STD".to_string(),
            Some(ModuleFuncs {
                functions: builtin_function_metas()
                    .iter()
                    .map(|meta| base_name(&meta.name).to_string())
                    .collect::<HashSet<_>>(),
            }),
        )]),
    };
    table.modules.insert(
        module_name.to_string(),
        Some(ModuleFuncs {
            functions: funcs
                .iter()
                .map(|registration| base_name(&registration.meta().name).to_string())
                .collect::<HashSet<_>>(),
        }),
    );
    let steps = match crate::parse_script_with_modules(example.code, table) {
        Ok(steps) => steps,
        Err(err) => return ExampleOutcome::Failed(format!("parse failed: {err:#}")),
    };
    let needle = match &example.fence_meta {
        Some(info) => match oxdock_parser::expect_error_from_info(&format!("oxdock {info}")) {
            Ok(needle) => needle,
            Err(err) => return ExampleOutcome::Failed(format!("bad fence metadata: {err:#}")),
        },
        None => None,
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let modules: Vec<HostModule<P>> = exec_modules.to_vec();
    std::thread::spawn(move || {
        // Real tempdir filesystem, mirroring docs_conformance: listing
        // calls (`GLOB`) read real disk through the `glob` crate, which
        // no in-memory mock backs. Examples stay hermetic by
        // self-seeding every file they touch.
        let outcome = (|| {
            let workspace_temp =
                GuardedPath::tempdir().map_err(|err| format!("fs setup: {err:#}"))?;
            let fs_root = workspace_temp.as_guarded_path().clone();
            let mut resolver = PathResolver::new_guarded(fs_root.clone(), fs_root.clone())
                .map_err(|err| format!("fs setup: {err:#}"))?;
            resolver.set_workspace_root(fs_root.clone());
            let fs: Box<dyn WorkspaceFs> = Box::new(resolver);
            crate::run_steps_with_manager_with_modules(
                fs,
                &steps,
                process,
                ExecIo::new(),
                modules,
                Vec::new(),
            )
            .map(|_| ())
            .map_err(|err| format!("{err:#}"))
        })();
        let _ = done_tx.send(outcome);
    });
    match done_rx.recv_timeout(std::time::Duration::from_secs(WATCHDOG_SECS)) {
        Ok(Ok(())) => match needle {
            Some(expected) => ExampleOutcome::Failed(format!(
                "was expected to fail with '{expected}' but succeeded"
            )),
            None => ExampleOutcome::Passed,
        },
        Ok(Err(text)) => match needle {
            Some(expected) if text.contains(&expected) => ExampleOutcome::ExpectedError,
            Some(expected) => {
                ExampleOutcome::Failed(format!("error did not contain '{expected}'; got: {text}"))
            }
            None => ExampleOutcome::Failed(format!("failed unexpectedly: {text}")),
        },
        Err(_) => ExampleOutcome::TimedOut,
    }
}

/// Identifier-boundary substring search: `INT` matches `INT($s)` but
/// never `PRINT` or `POINT`.
fn mentions(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let end = abs + needle.len();
        let boundary = |b: Option<u8>| {
            b.map(|c| !(c.is_ascii_alphanumeric() || c == b'_'))
                .unwrap_or(true)
        };
        let before = if abs == 0 { None } else { Some(bytes[abs - 1]) };
        if boundary(before) && boundary(bytes.get(end).copied()) {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Mock execution state mirroring the test harness (`exec::tests`):
/// empty mock filesystem, fresh builtins registry, one global scope.
/// Filesystem and process boundaries virtualize at the manager layer.
fn mock_state<P: ProcessManager>() -> ExecState<P> {
    let fs = MockFs::new();
    let mut state = ExecState {
        fs: Box::new(fs.clone()),
        cargo_scratch: oxdock_fs::reserve_cargo_scratch().expect("cargo scratch"),
        seed: Arc::new(HashMap::new()),
        cwd: fs.root().clone(),
        envs: Arc::new(HashMap::new()),
        bg_children: Vec::new(),
        scope_stack: Vec::new(),
        isolated_stack: Vec::new(),
        io: ExecIo::new(),
        assert_windows: Arc::new(std::sync::Mutex::new(HashMap::new())),
        assert_windows_stderr: Arc::new(std::sync::Mutex::new(HashMap::new())),
        exact_stdout: Arc::new(std::sync::Mutex::new(HashMap::new())),
        var_scopes: Vec::new(),
        cancel_token: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_hub: Arc::new(oxdock_process::CancelHub::new()),
        active_process: Arc::new(std::sync::Mutex::new(None)),
        named_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        next_task_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        live_tasks: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        inside_async: false,
        keeper_expiry: None,
        cancellable: false,
        functions: super::native::FunctionRegistry::with_builtins(),
        types: super::typing::startup_type_map(),
        record_schemas: HashMap::new(),
        module_states: HashMap::new(),
        type_aliases: HashMap::new(),
        call_depth: 0,
        task_id: 0,
        push_manifest: Vec::new(),
        _marker: std::marker::PhantomData,
    };
    state.push_var_scope();
    state
}

/// All deterministic sampling vectors for one signature: one base
/// vector (first candidate per position), one vector per extra union
/// member, and the options cases substituted into the base. Linear by
/// construction: positions never cross-multiply.
fn sample_vectors(meta: &super::native::FuncMeta) -> Vec<Vec<Value>> {
    let params = meta.params.as_deref().unwrap_or(&[]);
    let mut base: Vec<Value> = Vec::with_capacity(params.len());
    let mut unions: Vec<(usize, Vec<Value>)> = Vec::new();
    let mut options_at: Vec<usize> = Vec::new();
    for (idx, param) in params.iter().enumerate() {
        if param.options.is_some() {
            options_at.push(idx);
            base.push(Value::map(BTreeMap::new()));
            continue;
        }
        let candidates = param_candidates(param);
        if candidates.is_empty() {
            return Vec::new();
        }
        base.push(candidates[0].clone());
        if candidates.len() > 1 {
            unions.push((idx, candidates));
        }
    }
    let mut out = vec![base.clone()];
    // Union extras vary one position at a time against the base.
    for (idx, candidates) in &unions {
        for extra in candidates.iter().skip(1) {
            let mut vector = base.clone();
            vector[*idx] = extra.clone();
            out.push(vector);
        }
    }
    // Options cases substitute into the base, one param at a time.
    for idx in options_at {
        let count = params[idx].options.unwrap_or(&[]).len();
        let cases = option_cases(&params[idx]);
        assert_eq!(
            cases.len(),
            1 + count,
            "option sampling is empty-map plus one single-key map per key, never a cross-product"
        );
        for case in cases {
            let mut vector = base.clone();
            vector[idx] = case;
            out.push(vector);
        }
    }
    out
}

/// Candidate values for one argument position: one per union member,
/// a single sample otherwise, none when the shape cannot sample
/// (handles, custom types). `#[values]` sets sample their first
/// admitted literal for the best domain hit rate.
fn param_candidates(param: &FuncParam) -> Vec<Value> {
    if let Some(values) = param.allowed.as_ref()
        && let Some(first) = values.first()
    {
        return vec![Value::string(first.to_string())];
    }
    let Some(tag) = param.param_type.as_ref() else {
        return Vec::new();
    };
    match tag {
        TypeTag::Union(_, members) => members.iter().filter_map(sample_tag).collect(),
        _ => sample_tag(tag).into_iter().collect(),
    }
}

/// Option cases for one options MAP: the empty map plus one
/// single-key map per declared key. Union values sample their first
/// member through the shared sampler, so the count stays exactly
/// `1 + keys` by construction.
fn option_cases(param: &FuncParam) -> Vec<Value> {
    let keys = param.options.unwrap_or(&[]);
    let mut cases = vec![Value::map(BTreeMap::new())];
    for key in keys {
        if let Some(value) = sample_tag(&key.value) {
            cases.push(Value::map(BTreeMap::from([(key.name.to_string(), value)])));
        }
    }
    cases
}

/// One sample word for a tag, or `None` when sampling cannot mint it
/// (handles, permits, custom payloads). Unions sample their first
/// valid member, so composites holding unions (`LIST<INT|STRING>`,
/// records with union fields) still construct instead of dropping
/// the whole function off the sampling path.
fn sample_tag(tag: &TypeTag) -> Option<Value> {
    match tag {
        TypeTag::String => Some(Value::string("x".to_string())),
        TypeTag::Int => Some(Value::int(1)),
        TypeTag::Float => Some(Value::float(1.5)),
        TypeTag::Bool => Some(Value::bool(true)),
        TypeTag::List => Some(Value::list(Vec::new())),
        TypeTag::Map => Some(Value::map(BTreeMap::new())),
        TypeTag::Path => {
            #[allow(clippy::disallowed_types)]
            let path = std::path::PathBuf::from("x");
            Some(Value::path(path))
        }
        TypeTag::Duration => Some(Value::duration(std::time::Duration::from_secs(1))),
        TypeTag::Pipe => Some(Value::pipe_fresh()),
        TypeTag::Any => Some(Value::string("x".to_string())),
        TypeTag::Union(_, members) => members.iter().find_map(sample_tag),
        TypeTag::ListOf(inner) => Some(Value::list(vec![sample_tag(inner)?])),
        TypeTag::MapOf(values) => Some(Value::map(BTreeMap::from([(
            "k".to_string(),
            sample_tag(values)?,
        )]))),
        TypeTag::Record(fields) => {
            let mut entries = BTreeMap::new();
            for field in fields.iter() {
                entries.insert(field.name.clone(), sample_tag(&field.ty)?);
            }
            Some(Value::map(entries))
        }
        TypeTag::Handle | TypeTag::Semaphore | TypeTag::Permit | TypeTag::Custom(_) => None,
    }
}
