# ri-agent

A Rust command-line agent that sends a prompt to an OpenRouter-compatible model and executes local tools to complete the task. Built with Tokio and `async-openai`, it supports reading files, writing files, and running Bash commands across multiple model turns.

The Cargo package and executable are currently named `codecrafters-claude-code`.

## Getting started

With Nix installed and flakes enabled, run from this repository:

```sh
nix develop
export OPENROUTER_API_KEY='your-api-key'
cargo run -- -p "Read Cargo.toml and explain this project's dependencies"
```

The development shell supplies Rust, Cargo, formatting and linting tools, and native build dependencies. It also sets `MODEL=minimax/minimax-m3:free`; override it with `--model` or set `MODEL` after entering the shell.

Without Nix, use Rust 1.96 or newer, Cargo, and the native dependencies required by OpenSSL, including `pkg-config`. Bash must be available on `PATH` for the Bash tool.

## Usage

```sh
cargo run -- --help
cargo run -- -p "Explain src/agent.rs" --model anthropic/claude-haiku-4.5
cargo run -- -p "Inspect the project" \
  --max-turns 10 --command-timeout 30 --max-output-bytes 16384
```

| Option | Purpose | Default |
| --- | --- | --- |
| `-p`, `--prompt` | Task to send to the model | Required |
| `--model` | Model identifier; overrides `MODEL` | `MODEL`, otherwise `anthropic/claude-haiku-4.5` |
| `--max-turns` | Maximum model turns | `20` |
| `--command-timeout` | Timeout per Bash call, in seconds | `60` |
| `--max-output-bytes` | Retained bytes per file read or Bash output stream | `32768` |

All numeric limits must be greater than zero. Truncated tool output includes an `[output truncated]` marker.

### Configuration

`OPENROUTER_API_KEY` is required. `OPENROUTER_BASE_URL` optionally replaces the default API endpoint, `https://openrouter.ai/api/v1`. Credentials are read from the environment; keep them out of committed files.

### Tools

- **Read** returns file contents, subject to the output limit.
- **Write** creates or overwrites a file. Its parent directory must already exist.
- **Bash** runs a command in a fresh, noninteractive shell and returns stdout, stderr, and exit status. Shell state does not persist between calls.

Consecutive Read calls can run concurrently, up to four at a time. Write and Bash calls execute sequentially. Tool errors are returned to the model so it can attempt recovery.

Tools use the process's working directory and permissions. The agent executes tool calls without an approval prompt or built-in filesystem sandbox; file contents and command output may be sent to the configured model provider.

## Development

Inside `nix develop`:

```sh
cargo build
cargo test
cargo test --test agent_loop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Unit tests live alongside the source. Integration tests in `tests/agent_loop.rs` run the CLI against a local mock HTTP server and do not require live API credentials. They do not verify live provider behavior.

Use `nix build` to build the packaged executable at `result/bin/codecrafters-claude-code`, and `nix fmt` to format Nix files.

## Project structure

- `src/main.rs`: CLI arguments and API client configuration.
- `src/agent.rs`: model/tool conversation loop.
- `src/message.rs`, `src/response.rs`, `src/response_processor.rs`: protocol types and response handling.
- `src/tools/`: tool registry and implementations.
- `src/limits.rs`: execution limits and output truncation.
- `tests/agent_loop.rs`: integration tests.
- `flake.nix`: Nix development and build setup.

See [AGENTS.md](AGENTS.md) for contributor guidelines.
