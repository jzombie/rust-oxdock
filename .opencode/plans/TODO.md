Add comment to https://github.com/jzombie/rust-oxdock/issues/164 (and possibly rename title) for outcome of `platform: / arch: everywhere`

Note, we're not implementing that entire ticket here, but are implementing the namespaced literals.

----

TODO: EXTREMELY IMPORTANT

Take on Option 3 (Statement-arg checking) next.

Architectural Rationale
Seal the Read Boundary First: Allowing statements like ECHO \(val or pure-template string interpolations to bypass static check evaluation leaves a hole in site-aware liveness and type validation. A variable declared under a guard could be referenced in an un-guarded ECHO\)var or interpolated string without triggering the static pass.

Solid Foundation for Generics: Layering Phase G generic type annotations (LIST, MAP<...>) on top of an analyzer that isn't inspecting 100% of statement-level variable reads creates tech debt that will require retrofitting argument-walker passes later.

Workflow Order:

Step 1: Complete Option 3 so every AST step kind and template expression participates in site-aware liveness and type verification.

Step 2: Draft the Issue 164 outcome comment (Option 2) once the guard and argument boundaries are closed.

Step 3: Move to Phase G generics (Option 1).
