/// Macro for defining the command pipeline.
///
/// Generates:
/// - `lower_command(name, raw_args)` — flag stripping + `CommandSpec::lower`
/// - `execute_command(step, cx)` — match on `StepKind` variants, call handlers
/// - `all_metadata()` — collect all command metadata for docs-gen
///
/// Each entry specifies a pattern (`$pat`), a `CommandSpec` implementor,
/// and a handler function. The pattern must match the `StepKind` variant
/// shape exactly (e.g., `Write { .. }`, `Workdir(..)`, `Cwd`).
///
/// Requires `anyhow` and `oxdock_parser` to be in scope at the call site.
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
                    let (flags, positional) = $crate::strip_flags(raw_args, &meta)?;
                    <$cmd_type as $crate::CommandSpec>::lower(flags, positional)
                } )*
                _ => ::anyhow::bail!("unknown command: {name}"),
            }
        }

        /// Execute a step by dispatching to the appropriate handler.
        pub fn execute_command<P: $crate::ProcessManager>(
            step: &$crate::StepKind,
            cx: &mut $crate::StepCtx<'_, P>,
        ) -> ::anyhow::Result<()> {
            match step {
                $( $crate::StepKind::$pat => $handler(step, cx), )*
                // ALL structural variants handled explicitly
                $crate::StepKind::WithIo { bindings, cmd } => $crate::handlers::with_io(cx, bindings, cmd),
                $crate::StepKind::WithIoBlock { bindings } => $crate::handlers::with_io_block(cx, bindings),
                $crate::StepKind::For { .. } => $crate::handlers::for_loop(cx, step),
                $crate::StepKind::If { .. } => $crate::handlers::if_then(cx, step),
                $crate::StepKind::Assign { .. } => $crate::handlers::assign(cx, step),
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
