OxDock's pipe architecture is engineered for low-level, zero-copy byte streaming across process boundaries without accumulating memory.

OxDock intentionally omits a raw text-to-variable loader to enforce the boundary between bounded, structured script state (vars) and unbounded byte streaming (WITH_IO).

By keeping byte streaming on low-level buffer pipes and reserving the interpreter heap strictly for small configuration state, OxDock guarantees deterministic performance and $O(1)$ memory overhead regardless of payload size.

* **Zero-Copy Byte Pipelines:** Steps like `READ`, `WRITE`, and `RUN` stream raw `&[u8]` bytes directly through kernel/pipe buffers. They bypass UTF-8 validation, string allocations, and heap reallocations entirely.
* **Bounded Interpreter Memory:** The script variable table (`vars`) only ever stores small configuration primitives and structured AST values. Streaming a 50 GB archive through `WITH_IO` uses the exact same memory footprint as processing a 1 KB text file.
* **No Heap Spikes or OOM Crashes:** Banning arbitrary file-to-variable buffering prevents runaway memory allocation and garbage collection/drop overheads during pipeline execution.
