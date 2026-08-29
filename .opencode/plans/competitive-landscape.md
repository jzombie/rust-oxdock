OxDock sits at the intersection of container build specifications, task runners, and shell test automation frameworks.

**Container & Build DSLs**

* **Dockerfile / Containerfile:** Provides the primary keyword lineage (`WORKDIR`, `ENV`, `RUN`, `COPY`). OxDock adopts Dockerfile imperative setup semantics but augments them with local filesystem operations, variable bindings (`LET`), and control flow (`FOR`).
* **Earthfile (Earthly):** A close architectural cousin. Earthly blends Dockerfile syntax (`COPY`, `RUN`) with Makefile-style task execution, scriptable arguments, loops, and explicit target scoping.

**Task Runners & Automation DSLs**

* **Just (Justfile):** Shares variable evaluation, parameter handling, and command execution primitives. Like OxDock, `just` allows platform-aware guards and task dispatch without requiring full shell overhead.
* **Starlark (Bazel/Buck):** Shares strict determinism and build-time tracking (`HASH_SHA256`, input tracing). While Starlark uses Python-like syntax, OxDock uses a flatter, line-oriented block DSL.

**Shell Testing Frameworks**

* **bats-core (Bash Automated Testing System):** Shares the verification domain. `bats` executes shell commands and evaluates exit codes, file states, and standard output (`ASSERT_STDOUT`, `ASSERT_FILE`, `ASSERT_ABSENT`).
* **ShellSpec:** Similar BDD-style shell verification DSL focusing on sandboxed command execution, environment isolation, and I/O stream captures.

**Declarative Provisioning DSLs**

* **HCL / HashiCorp Packer:** Shares structured block scope semantics (`{}`), variable interpolation, environment overrides, and platform/environment guards (`[env:FOO == bar]`).
