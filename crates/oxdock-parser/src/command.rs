use crate::ast::{Arg, StepKind};
use anyhow::Result;

/// Metadata for a single command argument.
pub struct ArgSpec {
    pub name: &'static str,
    pub arg_type: &'static str,
    pub description: &'static str,
    pub io: IoDirection,
    pub index: usize,
    pub required: bool,
    pub fallback_stream: Option<Stream>,
}

/// Data direction for an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDirection {
    Read,
    Write,
}

/// Stream type for fallback or default output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

/// Metadata for a single flag.
pub struct FlagSpec {
    pub name: &'static str,
    pub long: &'static str,
    pub value_type: FlagValueType,
    pub required: bool,
    pub description: &'static str,
}

/// Type of value a flag accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagValueType {
    /// Boolean flag (no value required).
    Flag,
    /// String-valued flag.
    String,
    /// Integer-valued flag.
    Int,
}

/// Complete metadata for a command.
pub struct CommandMeta {
    pub name: &'static str,
    pub syntax: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub args: &'static [ArgSpec],
    pub flags: &'static [FlagSpec],
    pub default_output: Option<Stream>,
    pub examples: &'static [Example],
}

/// An executable example for a command.
pub struct Example {
    pub name: &'static str,
    pub fence_meta: Option<&'static str>,
    pub code: &'static str,
}

/// Trait for command metadata and lowering. No execution types.
///
/// This trait lives in `oxdock-parser` and has zero dependencies on
/// `oxdock-core`. Execution dispatch is handled separately by the
/// `define_pipeline!` macro in `oxdock-core`.
pub trait CommandSpec {
    const NAME: &'static str;

    fn metadata() -> CommandMeta;
    fn lower(flags: Vec<(String, Arg)>, args: Vec<Arg>) -> Result<StepKind>;
}
