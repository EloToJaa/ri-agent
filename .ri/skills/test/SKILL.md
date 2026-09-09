---
name: test
description: Validate this Rust harness with focused tests, workspace tests, formatting, and Clippy.
---

Validate the current changes, or the area specified by the user.

1. Read the repository instructions and inspect the changed files.
2. Run focused tests first if the scope is clear.
3. Run the following checks from the repository root:
   - `nix develop -c cargo fmt --all --check`
   - `nix develop -c cargo test --workspace`
   - `nix develop -c cargo clippy --workspace --all-targets -- -D warnings`
4. Report which checks passed or failed. Distinguish pre-existing failures from regressions when evidence supports it.
5. Do not claim live-provider compatibility based on mock tests. Do not change files to fix failures unless requested.
