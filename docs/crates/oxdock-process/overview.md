Process orchestration layer. `CommandBuilder` constructs cross-platform
commands with env expansion, shell resolution, and background-mode support.
Under Miri the process manager is replaced by a synthetic implementation so
tests stay hermetic.
