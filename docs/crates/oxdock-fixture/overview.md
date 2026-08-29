Materializes a Cargo project template inside a temporary, auto-cleaned
directory for integration tests. Workspace dependencies are patched to local
paths at runtime so the fixture builds against the current checkout.
