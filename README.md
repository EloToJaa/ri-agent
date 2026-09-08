# ri-agent

An asynchronous Rust agent harness with a Ratatui terminal interface, a one-shot CLI, and Lua configuration/hooks/custom tools. It uses an OpenRouter-compatible API and can read files, write files, and execute Bash locally.

## Getting started

```sh
nix develop
export OPENROUTER_API_KEY='your-api-key'
cargo run -p ri-agent-cli                     # Interactive TUI
cargo run -p ri-agent-cli -- -p "Explain crates/agent/src/agent.rs"  # One-shot CLI
cargo run -p ri-agent-cli -- --tui -p "Inspect this project"
```

The executable retains its original name, `codecrafters-claude-code`. Without Nix, install Rust 1.96+, a C compiler (for vendored Lua), and Bash. The Nix shell sets `MODEL=minimax/minimax-m3:free`; unset it to use the model from Lua, or override it with `--model`.

### Interactive interface

The TUI keeps conversation history across prompts and shows assistant responses, tool results, and model/tool activity. Responses appear when complete, not token-by-token.

- **Enter**: send a prompt when idle. You can draft the next prompt while the agent works.
- **Backspace**: delete the last character/grapheme.
- **Page Up / Page Down**: scroll the transcript.
- **Ctrl+L**: start a new conversation when idle.
- **Ctrl+C**: quit and drop the active agent turn. Already-applied tool effects remain.

The visible transcript retains up to 4,000 lines; model conversation history remains intact until cleared. Failed turns are removed from model history, without undoing local tool effects. The TUI requires a terminal; use `-p` for scripts and pipes.

## Lua configuration

The harness loads `$XDG_CONFIG_HOME/ri-agent/config.lua`, or `~/.config/ri-agent/config.lua` when XDG_CONFIG_HOME is unset. A missing default file is fine. `--config PATH` loads an explicit file and reports missing-file or Lua errors. Project-local configuration is **never loaded automatically**.

Start with [examples/config.lua](examples/config.lua):

```sh
mkdir -p ~/.config/ri-agent
cp examples/config.lua ~/.config/ri-agent/config.lua
cargo run -p ri-agent-cli -- --config examples/config.lua
```

The file returns a table with:

- `settings`: `model`, `base_url`, `max_turns`, `command_timeout`, `max_output_bytes`.
- `hooks.before_prompt(text)`: transform each submitted user prompt.
- `hooks.after_response(text)`: transform assistant text before display and storage, including text accompanying tool calls.
- `tools`: an array of `{ name, description, parameters, execute }`. `parameters` is an object JSON schema; `execute(args)` receives decoded JSON arguments. Return a string or a JSON-serializable Lua value. Validate arguments inside the tool; schemas are advertised to the model, not enforced locally. Names cannot shadow built-ins or each other.

Hooks return a replacement string or `nil` to leave text unchanged. Hook errors fail the current turn. Tool errors are returned to the model for recovery. Custom tool results are capped by `max_output_bytes` before entering model history.

**Lua is trusted executable code, not a sandbox.** It can access local files, environment variables, and processes. Callbacks run on blocking threads, but have no execution timeout and cannot be forcibly cancelled; a blocked callback can delay process exit. `command_timeout` applies only to built-in Bash. Keep callbacks finite, avoid terminal writes (`print`, `io.write`, subprocess output) while using the TUI, and do not load untrusted configuration. No credentials belong in configuration files; use `OPENROUTER_API_KEY`.

### Options and precedence

CLI flags override environment variables (`MODEL`, `OPENROUTER_BASE_URL`), which override Lua settings, which override built-in defaults.

| Option | Default |
| --- | --- |
| `-p`, `--prompt` | No prompt opens the TUI |
| `--tui` | Force TUI, optionally with an initial `-p` prompt |
| `--config` | User config path described above |
| `--model` | `anthropic/claude-haiku-4.5` |
| `--base-url` | `https://openrouter.ai/api/v1` |
| `--max-turns` | `20` per submitted prompt |
| `--command-timeout` | `60` seconds per Bash call |
| `--max-output-bytes` | `32768` retained bytes per Read, Bash stream, or Lua tool result |

All numeric limits must be positive. Truncation adds an `[output truncated]` marker.

## Tools and security

- **Read**: read a file, subject to the output limit.
- **Write**: create or overwrite a file; the parent directory must exist.
- **Bash**: run a command in a fresh noninteractive shell, returning stdout, stderr, and exit status. Shell state does not persist.

Consecutive Read calls run concurrently (up to four). Mutating and Lua tools run sequentially. Tools operate with the process's working directory and permissions, **without approval prompts or filesystem sandboxing**. File contents and command output can be sent to the model provider.

## Workspace

```text
crates/agent/  ri-agent       Main library: sessions, protocol, events, built-in tools
crates/lua/    ri-agent-lua   Lua settings, hooks, custom-tool runtime
crates/tui/    ri-agent-tui   Ratatui interface using the main library
crates/cli/    ri-agent-cli   CLI configuration and interface selection
```

The main library re-exports the Lua crate as `ri_agent::config`. Frontends use `Session` and an event channel (`Output::channel`); `Output::default` writes CLI output. Sessions own conversation history and expose `submit` and `clear`.

## Development

Inside `nix develop`:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests cover local tools, Lua validation/hooks/tools, TUI state/rendering, and mock-HTTP agent sessions. They need no live credentials and do not verify live-provider compatibility. `nix build` packages `result/bin/codecrafters-claude-code`; run `nix fmt` after editing Nix files.
