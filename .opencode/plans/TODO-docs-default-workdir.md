> what's the default workdir, by the way? local?

By default, `oxdock` sets `WORKDIR` to the **root (`/`) of an isolated, temporary workspace directory**, not your live `LOCAL` repository.

* **Hermetic Sandbox:** Scripts execute inside an auto-cleaned temporary directory to keep build side effects and generated files from dirtying your host source checkout.
* **Relative Pathing:** Path resolutions start at the top level of this temporary sandbox environment.
* **Explicit Local Override:** If a script must modify or read your actual host repository directly, you explicitly switch execution targets using `WORKSPACE LOCAL`.
