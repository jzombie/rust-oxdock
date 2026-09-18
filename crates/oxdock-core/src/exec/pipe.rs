use oxdock_pipe::ScriptPipeEndpoint;
use oxdock_process::SharedOutput;

use super::io::StreamHandle;

/// Backend home: the script-pipe backend lives in `oxdock-pipe` so `PIPE`
/// values can own handles without a dependency cycle. Resolution, keepers,
/// and diagnostics below operate on those backends; this module keeps only
/// the core-side endpoint and inspect types.
#[derive(Clone)]
pub(crate) enum PipeEndpoint {
    Stream(SharedOutput),
    Script(ScriptPipeEndpoint),
    Inherit,
}

impl PipeEndpoint {
    pub(super) fn stream(writer: SharedOutput) -> Self {
        PipeEndpoint::Stream(writer)
    }

    pub(super) fn script(endpoint: ScriptPipeEndpoint) -> Self {
        PipeEndpoint::Script(endpoint)
    }

    pub(super) fn to_stream_handle(&self) -> StreamHandle {
        match self {
            PipeEndpoint::Stream(writer) => StreamHandle::Stream(writer.clone()),
            PipeEndpoint::Script(endpoint) => StreamHandle::Stream(endpoint.stream_handle()),
            PipeEndpoint::Inherit => StreamHandle::Inherit,
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct PipeOutputs {
    pub(super) stdout: Option<PipeEndpoint>,
    pub(super) stderr: Option<PipeEndpoint>,
}

/// Snapshot of one pipe backend for `INSPECT()` diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PipeKindDesc {
    /// In-memory store-and-forward buffer (possibly spilled to disk).
    Script,
    /// Zero-copy OS kernel pair. Kernel-side bytes are invisible, so
    /// buffered counts stay 0 and reader/writer counts report pair
    /// presence, not live takes. Never constructed under Miri, where
    /// promotion is compiled out.
    #[cfg_attr(miri, allow(dead_code))]
    Os,
    /// Host-injected raw handle with no script backend.
    External,
    /// No entry under this name.
    Missing,
}

impl PipeKindDesc {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            PipeKindDesc::Script => "script",
            PipeKindDesc::Os => "os",
            PipeKindDesc::External => "external",
            PipeKindDesc::Missing => "missing",
        }
    }

    pub(super) fn is_os(self) -> bool {
        matches!(self, PipeKindDesc::Os)
    }
}

/// Point-in-time pipe stats. Backend handles are cloned out from under the
/// registry lock, then queried, so no lock is ever held across both.
#[derive(Debug, Clone)]
pub(super) struct PipeInfo {
    pub(super) kind: PipeKindDesc,
    /// Bytes currently buffered for script pipes; always 0 for OS pairs
    /// (kernel bytes are invisible) and external handles.
    pub(super) buffered: u64,
    /// Registered readers: 1 when an input handle exists, else 0.
    /// OS pairs report presence, not live takes.
    pub(super) readers: usize,
    /// Live data-writer attachments for script pipes (keeper pins excluded);
    /// OS pairs report presence, not live takes.
    pub(super) writers: usize,
}
