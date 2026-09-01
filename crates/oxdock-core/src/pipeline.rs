/// Macro for defining the command pipeline.
///
/// Generates:
/// - `lower_command(name, raw_args)` — flag stripping + `CommandSpec::lower`
/// - `execute_command(step, cx)` — match on `StepKind` variants, call handlers
/// - `all_metadata()` — collect all command metadata for docs-gen
///
/// Each entry specifies a pattern (`$pat`), a `CommandSpec` implementor,
/// and a handler function with signature `fn(&StepKind, &mut StepCtx<'_, P>) -> Result<()>`.
#[macro_export]
macro_rules! define_pipeline {
    (
        $( $pat:pat => ($cmd_type:ty, $handler:path) ),* $(,)?
    ) => {
        /// Lower a command by name: strip flags, then call `CommandSpec::lower`.
        pub fn lower_command(
            name: &str,
            raw_args: Vec<$crate::Arg>,
        ) -> ::anyhow::Result<$crate::StepKind> {
            match name {
                $( s if s == <$cmd_type as $crate::CommandSpec>::NAME => {
                    let meta = <$cmd_type as $crate::CommandSpec>::metadata();
                    let (flags, positional) = oxdock_parser::strip_flags(raw_args, &meta)?;
                    <$cmd_type as $crate::CommandSpec>::lower(flags, positional)
                } )*
                _ => ::anyhow::bail!("unknown command: {name}"),
            }
        }

        /// Execute a step by dispatching to the appropriate handler.
        pub fn execute_command<P: $crate::ProcessManager>(
            step: &$crate::StepKind,
            cx: &mut $crate::exec::StepCtx<'_, P>,
        ) -> ::anyhow::Result<()> {
            match step {
                $( $pat => $handler(step, cx), )*
                $crate::StepKind::WithIo { bindings, cmd } => $crate::exec::with_io(cx, 0, 0, bindings, cmd),
                $crate::StepKind::WithIoBlock { bindings } => $crate::exec::with_io_block(cx, 0, 0, bindings),
                $crate::StepKind::InheritEnv { .. } => $crate::exec::dispatch_inherit_env(step, cx),
                $crate::StepKind::For { .. } => $crate::exec::dispatch_for_loop(step, cx),
                $crate::StepKind::If { .. } => $crate::exec::dispatch_if_then(step, cx),
                $crate::StepKind::Assign { .. } => $crate::exec::dispatch_assign(step, cx),
            }
        }

        /// Collect metadata from all registered commands.
        pub fn all_metadata() -> Vec<$crate::CommandMeta> {
            vec![
                $( <$cmd_type as $crate::CommandSpec>::metadata(), )*
            ]
        }
    };
}
