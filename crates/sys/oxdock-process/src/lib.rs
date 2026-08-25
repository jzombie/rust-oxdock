mod builder;
pub mod builtin_env;
mod child;
mod contract;
mod expand;
#[cfg(feature = "mock-process")]
mod mock;
pub mod serial_cargo_env;
mod shell;
mod shell_manager;
#[cfg(miri)]
mod synthetic;

pub use builder::{CommandBuilder, CommandOutput, CommandSnapshot};
pub use builtin_env::BuiltinEnv;
pub use child::ChildHandle;
pub use contract::{
    BackgroundHandle, CommandContext, CommandMode, CommandOptions, CommandResult, CommandStderr,
    CommandStdout, ProcessManager, SharedInput, SharedOutput,
};
pub use expand::{expand_command_env, expand_script_env};
pub use oxdock_sys_test_utils::TestEnvGuard;
pub use shell::{ShellLauncher, shell_program, spawn_interactive_shell};
pub use shell_manager::ShellProcessManager;

#[cfg(feature = "mock-process")]
pub use mock::{MockHandle, MockProcessManager, MockRunCall, MockSpawnCall, MockStreamMode};

#[cfg(miri)]
pub use synthetic::{SyntheticBgHandle, SyntheticProcessManager};

#[cfg(not(miri))]
pub type DefaultProcessManager = ShellProcessManager;

#[cfg(miri)]
pub type DefaultProcessManager = SyntheticProcessManager;

pub fn default_process_manager() -> DefaultProcessManager {
    DefaultProcessManager::default()
}
