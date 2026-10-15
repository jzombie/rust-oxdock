mod builder;
pub mod builtin_env;
mod child;
mod contract;
pub mod env_policy;
mod expand;
#[cfg(feature = "mock-process")]
mod mock;
pub mod serial_cargo_env;
mod shell;
mod shell_manager;
#[cfg(miri)]
mod synthetic;

pub use builder::{CommandBuilder, CommandOutput, CommandSnapshot, PipedChild};
pub use builtin_env::BuiltinEnv;
pub use child::ChildHandle;
pub use contract::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdin, CommandStdout, INHERIT_STDOUT_ENV_VAR, PROCESS_DEBUG_ENV_VAR, ProcessManager,
    SharedInput, SharedOutput,
};
#[cfg(not(miri))]
pub use contract::{OsPipeReader, OsPipeWriter, create_os_pipe};
pub use env_policy::{
    check_spawn_anchors, has_var, is_sanctioned_seed, names_eq, needs_path_lookup, seed_defaults,
    seed_differs, seed_transport,
};
pub use expand::{
    PlaceholderCall, StreamingExpand, expand_command_env, expand_script_env,
    format_value_for_string, parse_placeholder_call, resolve_placeholder_arg,
};
pub use oxdock_sys_test_utils::TestEnvGuard;
pub use shell::{ShellLauncher, batch_shell, shell_program, spawn_interactive_shell};
pub use shell_manager::ShellProcessManager;

#[cfg(feature = "mock-process")]
pub use mock::{
    MockHandle, MockProcessManager, MockRunArgvCall, MockRunCall, MockSpawnArgvCall, MockSpawnCall,
    MockStreamMode,
};

#[cfg(miri)]
pub use synthetic::{SyntheticBgHandle, SyntheticProcessManager};

#[cfg(not(miri))]
pub type DefaultProcessManager = ShellProcessManager;

#[cfg(miri)]
pub type DefaultProcessManager = SyntheticProcessManager;

pub fn default_process_manager() -> DefaultProcessManager {
    DefaultProcessManager::default()
}
