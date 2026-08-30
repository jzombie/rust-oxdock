> **OxDock is an experimental DSL used for building embeddable artifacts and orchestrating pipelines.**
>
> **It is currently in alpha and is subject to rapid API changes.**

# OxDock

OxDock is a Dockerfile-inspired DSL that runs **natively on your host** — no containers, no daemon, no VM. It comes in two flavors sharing one core: a [Rust build-time macro](./oxdock-macros/) whose scripts run during compilation, embedding resources directly into the binary's data section (no heap allocation when the program starts; the generated asset structs are pure Rust and work in `no_std` targets), and a [standalone CLI](./oxdock-cli/) that orchestrates cross-platform workflows as ordinary local processes.

Unlike Docker, commands execute directly on the host: they can be guarded by platform/env conditions, run inside scoped blocks so changes to `ENV` or `WORKDIR` don’t leak, and interoperate with containers whenever you want them — you can invoke Docker from an OxDock script, or even install Docker, while the DSL itself stays portable.
