# Repository Guidelines

## Project Structure & Module Organization

This repository implements an asynchronous Rust agent harness using an OpenRouter-compatible API. The workspace contains `ri-agent` (main library), `ri-agent-lua`, `ri-agent-tui`, and `ri-agent-cli`. The executable is named `ri`.

- `crates/cli/src/main.rs` defines CLI arguments, configuration precedence, and frontend selection.
- `crates/tui/src/lib.rs` owns Ratatui rendering and terminal input.
- `crates/lua/src/lib.rs` loads trusted user configuration and runs hooks/custom tools.
- `crates/agent/src/lib.rs` exposes sessions, events, limits, and Lua configuration.
- `crates/agent/src/agent.rs` manages the conversation loop; `message.rs`, `response.rs`, and `response_processor.rs` define and process protocol data.
- `crates/agent/src/tools/` contains the `Tool` trait, shared registry, and Read, Write, and Bash implementations. Register new tools in `TOOLS` so definitions and execution stay aligned.
- `crates/agent/src/limits.rs` centralizes execution limits.
- Unit tests live alongside implementation code; `crates/cli/tests/agent_loop.rs` exercises the binary against a local mock HTTP server.
- `flake.nix` supplies the development shell, package build, and Nix formatter.

## Build, Test, and Development Commands

Enter `nix develop` for Cargo, Rust, rustfmt, Clippy, and native dependencies. Run these commands inside that shell:

```sh
cargo build                            # Build the CLI
cargo test                             # Run unit and integration tests
cargo test --test agent_loop           # Run mock API integration tests
cargo fmt --check                      # Check Rust formatting
cargo clippy --all-targets -- -D warnings
cargo run -p ri-agent-cli -- -p "Describe this project" # Run using configured credentials
```

Use `nix build` for the Nix package and `nix fmt` after editing Nix files.

## Coding Style & Naming Conventions

Use Rust 2024 conventions and the declared minimum Rust version, 1.96. Follow rustfmt's four-space indentation. Use `snake_case` for functions and modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer guard clauses and early returns over `if/else` nesting. Preserve typed protocol boundaries and add contextual errors with `anyhow`.

## Testing Guidelines

Use `#[test]` for synchronous tests and `#[tokio::test]` for asynchronous behavior. Name tests after observable behavior. Cover relevant error recovery, limits, and tool ordering when changing those paths. Keep tests independent of live credentials using the existing mock server. No numeric coverage threshold is configured; local tests do not establish live-provider compatibility.

## Commit & Pull Request Guidelines

Existing commits use `feat(proj): description`; follow `type(scope): description` for focused changes. PRs should explain the problem, resulting behavior, and validation performed. Link relevant issues and identify any unverified runtime behavior.

## Security & Configuration

Set `OPENROUTER_API_KEY` through the environment; never commit credentials. `MODEL` or `--model` selects the model, and `OPENROUTER_BASE_URL` overrides the endpoint. The Nix shell does not set `MODEL`; defaults are provider-specific. Read, Write, and Bash operate locally, so run prompts from an appropriate working directory.
