# ri-agent

An asynchronous Rust agent harness with a Ratatui terminal interface, a one-shot CLI, and Lua configuration/hooks/custom tools. It supports OpenRouter and OpenAI Codex (with ChatGPT OAuth) and can read files, write files, and execute Bash locally.

## Getting started

Use `ri --ask` to approve each Write, Edit, Bash, or custom tool call. The TUI shows the arguments and accepts `y` to approve or `n`/Escape to deny; arrow/Page keys scroll the transcript and Ctrl+C cancels the turn. The one-shot CLI requires terminal stdin and the exact answer `yes`, with prompts on stderr (including under `--json`). Non-terminal input and disconnected approval responders deny execution. Read, Search, and Find do not require approval. `--read-only` takes precedence over `--ask`. These policies govern tool calls; trusted Lua configuration and hooks still run locally.

With Nix, launch the packaged TUI directly (no development shell needed):

```sh
nix run . -- login                 # Browser PKCE login; saves ~/.ri/credentials.json
# or: nix run . -- login --api-key   # Hidden prompt for an existing OpenRouter key
# or: nix run . -- login --manual    # Paste the browser callback URL into the terminal
# or: nix run . -- login --provider openai-codex # ChatGPT OAuth for Codex
# or: nix run . -- login --provider openai-codex --manual # Paste the Codex callback URL
nix run
nix run . -- --resume                 # Resume this directory's latest session
nix run . -- --no-save                # Try the TUI without saving history
nix run . -- --help
```

The first run builds the application and its vendored Lua/SQLite dependencies. Bash, ripgrep (`rg`), and `fd` are included in the packaged runtime; other commands invoked by the agent use your existing `PATH`. Use F2 for models, F3 for reasoning, and F4 for saved sessions.

For an installed binary, use `ri login` directly. For development or a fresh checkout, use `nix run . -- login` as shown above.

For development:

```sh
nix develop
export OPENROUTER_API_KEY='your-api-key'
cargo run -p ri-agent-cli                     # Interactive TUI
cargo run -p ri-agent-cli -- -p "Explain crates/agent/src/agent.rs"  # One-shot CLI
cargo run -p ri-agent-cli -- --tui -p "Inspect this project"
```

The executable is named `ri`. `ri login` authenticates with OpenRouter; `ri login --provider openai-codex` authenticates with ChatGPT OAuth for Codex. The default provider is OpenRouter and `--provider openai-codex` selects Codex. OpenRouter login using a browser-based PKCE flow and a localhost callback. On headless or restricted systems, use `ri login --api-key` to paste a key without displaying it, or `ri login --manual` to open the browser and paste the complete redirected callback URL back into the terminal. For Codex on headless systems or when port 1455 is unavailable, use `ri login --provider openai-codex --manual`. Open the printed URL, approve login, then copy the complete `http://localhost:1455/auth/callback?...` address from the browser—even if that page fails to load—and paste it into the hidden terminal prompt. Codex manual mode does not bind a callback port and validates the callback destination and state before exchanging the code. Manual login retains S256 PKCE; never share or save the callback URL. Codex does not support API-key login. Without Nix, install Rust 1.96+, a C compiler (for vendored Lua), Bash, ripgrep (`rg`), and `fd` (available as `fd` on `PATH`). The Nix shell does not force `MODEL`, so each provider can choose an appropriate default. If an older shell exported an OpenRouter model, run `unset MODEL` before selecting Codex, or pass an explicit Codex `--model`.

Automatic compaction runs before each model request when estimated conversation history reaches 32,000 tokens. Set `--auto-compact-tokens N` to change the soft threshold, or `0` to disable it. Summaries group requests/constraints, assistant decisions/progress, and observed tool results, preserving both beginnings and endings. Before summarizing, the full history is archived and its path included for retrieval with Read or Search. Large tool results can also be archived within an active turn, preserving the prompt and tool-call/result pairs. Persistent archives live beside the SQLite database in a `.artifacts` directory; `--no-save` uses private temporary archives removed when the session drops. Archives can contain the same sensitive content as history.

Context checks include serialized tool definitions, `--response-reserve` tokens (default 8,192), and 1,024 tokens of protocol headroom. A loaded model catalog supplies the context window when advertised; `--context-window N` overrides it when needed. Without catalog metadata or an override, only the soft threshold applies. Estimates still use serialized bytes divided by four, so they are approximate. If retained instructions and recent history exceed a known window after compaction, the request fails locally with a context-budget explanation. Setting the soft threshold to zero does not disable a known model's hard budget. Compaction progress appears in CLI, TUI, and JSON events.

### Interactive interface

The TUI uses a responsive agent-workbench layout: a provider/session rail, persistent model and reasoning state, a bordered transcript, an activity-aware composer, and searchable modal selectors. It keeps conversation history across prompts and shows assistant responses, tool results, and model/tool activity. OpenRouter and Codex response text streams into the transcript as it arrives; the one-shot CLI also streams to stdout. Completed text replaces the provisional TUI preview without duplication. Failed streams are marked as incomplete in the TUI and are not saved as completed responses. Tool calls execute only after the response completes successfully. OpenRouter-compatible endpoints that return JSON still work, with buffered output.

- **Ctrl+D**: quit. **Ctrl+C** quits when idle; during a turn it requests cancellation and keeps the TUI open.
- **Esc**: cancel the active turn when no picker or completion is open. Pickers and completions consume Esc first. Cancellation stops model requests immediately; active local tools and Lua hooks finish before the turn stops, and remaining tools are skipped. Bash still uses its configured timeout; a blocked Lua callback can delay cancellation. Applied changes are retained.
- **F2**: search the active provider’s live model catalog. Switching models resets the reasoning override and removes old model-specific reasoning metadata, but preserves conversation text and tool results.
- **F3**: select reasoning effort or **Provider default**. Choices follow the selected model’s `reasoning.supported_efforts`; mandatory reasoning models never offer `none`. Missing capability metadata only offers provider defaults, with a label explaining whether the catalog is loading/unavailable, the selected model is absent, or effort levels are not advertised. An open picker refreshes when the catalog arrives.
- **F4**: search and resume saved sessions for this working directory. Modal selectors support wrapping Up/Down navigation, Page Up/Page Down jumps, filtering, match counts, and clear empty states.
- **`/compact`**: replace older turns with a local excerpt summary capped at 8,192 characters, prioritizing recent excerpts. Retain at least six recent messages and extend backwards to a user turn so tool calls stay with their results. If there is no useful boundary, leave history unchanged. A failed save restores the original history.
- **`/fork`**: save the current conversation and continue it under a new session ID; the parent remains available in the session picker.
- **`--json`**: emit newline-delimited JSON events for one-shot scripts and editor integrations.
- **`--read-only`**: permit Read, Search, and Find while blocking Bash, Write, Edit, and Lua custom tools at the execution boundary.
- **Provider selection**: `/provider` opens the provider picker; `/provider openrouter` or `/provider openai-codex` switches directly. Switching loads environment/saved credentials, starts a new conversation with that provider’s default model and reasoning, and refreshes its catalog. Codex chooses an advertised model from its live catalog rather than relying on a retired default slug. Previous saved sessions remain available: switch back to their provider before resuming them. Missing credentials or a failed checkpoint leaves the current conversation unchanged. Authenticate first with `ri login --provider <id>`. The configured OpenRouter base URL is preserved when switching back.
- **Slash commands**: `/provider [id]`, `/model [query]`, `/reasoning [effort]`, `/resume [id]`, `/login [--manual]`, and `/help`. Typing `/` opens command suggestions; Up/Down selects a command, Tab completes it, Enter completes a partial command or executes an exact command, and Esc dismisses the input. Commands can be typed or pasted into the prompt; `/model`, `/reasoning`, and `/resume` with no argument open their pickers. `/login --manual` uses the active provider’s manual flow (for first-time Codex authentication, run the CLI command above before switching providers). `/login` temporarily suspends the TUI and its input reader, runs `ri login` in normal terminal mode, then restores and fully redraws the TUI. Ctrl+C cancels login and returns to the conversation; login errors also return without losing history or draft text. After a successful login, restart the TUI to use the saved credential because credentials are fixed when a session starts.
- **F5**: refresh the catalog after a network error. The configured model still works without a catalog when using provider-default reasoning.
- **$skill-name** at the start of a prompt or after whitespace: autocomplete installed skills inline, like slash commands. Up/Down selects, Tab completes, Enter completes a partial name or submits an exact name, and Esc cancels the current completion. Skill instructions are loaded on submission; you can combine skills and text, e.g. `$review focus on persistence` or `Check this change with $test`.
- **@** at the start of a prompt or after whitespace: open a searchable file picker backed by `fd` in the current working directory. Type or paste to filter, use Up/Down to select, Enter to insert a reference such as `@src/main.rs` (no quotes or leading `./`; spaces and backslashes are escaped), and Esc to cancel. Selection inserts a path, not file contents; the agent can use Read to inspect it. Discovery respects ignore files and excludes hidden files, runs asynchronously with a 10-second timeout, and retains at most 1 MiB of paths (with a truncation notice).
- **Enter**: send a prompt when idle. During a turn, send a correction by pressing Enter: this requests cancellation and queues one correction to run after the current turn is saved. Further drafts remain in the composer while that correction is queued. Ctrl+C/Esc restores a queued correction to the draft instead of sending it. A turn or persistence error also keeps the correction in the draft. Model, reasoning, and session changes are only available when idle.
- **Backspace**: delete the last character/grapheme.
- **Page Up / Page Down**: scroll the transcript.
- **Ctrl+L**: start a new conversation when idle.
Quitting drops the active agent turn; the SQLite checkpoint remains resumable and local tool effects already applied remain.

The visible transcript retains up to 4,000 lines; model conversation history remains intact until cleared. Failed turns retain completed tool work and a failure notice. Turn limits pause the session; other errors mark it failed. A failure before any tool work removes the failed prompt. The TUI requires a terminal; use `-p` for scripts and pipes.

Provider requests retry up to two times after transient connection, timeout, rate-limit, or 5xx failures, with 100 ms and 200 ms backoff. Retry progress is emitted as an activity event. Authentication, malformed-response, model-access, and other non-transient errors fail immediately. A cancelled request interrupts both the request and its backoff; no tool call is executed until a complete provider response is received.

Orderly cancellation preserves the submitted prompt, completed responses and tool results, explicit `executed=false` results for skipped calls, and a harness cancellation notice. Partial streamed text stays visibly incomplete and is excluded from model history. The saved conversation can be resumed without replaying tools. Abrupt exits recover the durable tool journal described below.

## SQLite sessions

All default harness files live under `~/.ri`: Lua configuration in `~/.ri/config.lua`, SQLite sessions in `~/.ri/sessions.sqlite3`, and OpenRouter credentials in `~/.ri/credentials.json`. Credentials use restrictive file permissions and are never stored in session records. `OPENROUTER_API_KEY` overrides the saved credential. The credentials file is plaintext; protect your home directory. The directory is created automatically when saving is enabled. Session lists are scoped to the canonical current working directory. SQLite stores prompts, assistant messages, tool results, reasoning metadata, and the selected model/effort; API client credentials and Lua code are not serialized. On Unix, new directories use mode `0700` and the database uses `0600`. Stored conversation/tool contents can themselves contain secrets; the database is **not encrypted**.

```sh
cargo run -p ri-agent-cli -- --sessions           # List this directory's sessions; no API key needed
cargo run -p ri-agent-cli -- --resume             # Resume latest in TUI
cargo run -p ri-agent-cli -- --resume SESSION_ID
cargo run -p ri-agent-cli -- --resume SESSION_ID -p "Continue with tests"
cargo run -p ri-agent-cli -- --no-save            # In-memory only
cargo run -p ri-agent-cli -- --session-db /path/to/sessions.sqlite3
```

Resume restores the saved model and reasoning choice rather than configuration defaults. New Lua configuration and limits apply when the process starts. Ctrl+L creates a new session without deleting the old one. Tools are never replayed during resume. Each tool batch records its intent before execution, and each mutating call records its result before the next call starts. Interrupted runs recover these records: completed results are retained, unstarted calls have `executed=false`, and calls interrupted before recording a result have `execution=unknown`. Inspect local state before retrying uncertain calls. Session status distinguishes ready, running, failed, and paused work. Legacy checkpoints retain their older recovery boundary. Optimistic revisions prevent simultaneous processes from silently overwriting the same session. Persistence errors stop the turn rather than silently losing history.

## Project instructions

Before each submitted prompt, the CLI and TUI reload `AGENTS.md` files from the nearest ancestor containing `.git` (a directory or worktree file) down to the launch directory, plus nested instruction files below the launch directory. Without a Git root, discovery starts at the launch directory. Sibling directories outside that subtree are not scanned.

Each document is labeled with its path and scope. Rules apply to its containing directory and descendants; deeper rules override ancestor rules only within their subtree. Explicit user requests take precedence. These are instructions for the model, not a filesystem permission boundary. Documents are appended after skill expansion and the Lua `before_prompt` hook, so mentions such as `$name` inside `AGENTS.md` do not invoke skills. Loading instructions executes no code and does not alter the files.

Discovery skips directory symlinks and `.git`, `.hg`, `.svn`, `target`, `node_modules`, `.direnv`, `.cache`, `.venv`, and `__pycache__` directories. It does not apply Git ignore rules. Instruction-file symlinks are supported and retain the scope of their containing directory. A maximum of 4,096 directories, 128 KiB per file, and 512 KiB of combined instruction context is allowed. Unreadable, invalid UTF-8, nonregular, or oversized instruction files fail the prompt before the model request instead of silently dropping rules.

Snapshots are saved with their prompts. Resume preserves those snapshots; subsequent prompts load fresh instructions, with the new snapshot explicitly superseding older project rules. Use `--no-project-instructions` to disable loading for an invocation. Library callers opt in with `Session::with_project_instructions(directory)`, using an absolute launch directory. For work outside the discovery scope, read the applicable instruction files explicitly.

## Skills

Skills are Markdown instructions invoked explicitly with `$name`, not executable plugins. The CLI and TUI discover skills at startup from:

- `~/.ri/skills/<directory>/SKILL.md` — personal skills.
- `.ri/skills/<directory>/SKILL.md` — skills in the current working directory; these override personal skills with the same name.

This repository includes `$review` and `$test` project skills. Install additional skills by creating a directory and a `SKILL.md` file:

```markdown
---
name: explain
description: Explain a code path and its important trade-offs.
---
Read the requested code. Explain the data flow, error paths, and relevant tests.
Do not modify files unless requested.
```

Names use 1–64 lowercase letters, digits, or hyphens, without leading/trailing hyphens. YAML frontmatter must contain a name and nonempty description; the Markdown body must also be nonempty. Only immediate child directories are scanned. Invalid skills are skipped with warnings; restart after installing or renaming a skill. Existing skill bodies are reread when invoked.

```sh
ri -p '$review the current changes'  # Single quotes prevent shell expansion
```

Mention multiple skills to combine their instructions; repeated mentions load a skill only once. Unknown `$names` fail before any model request. Escape a literal reference as `\$name`; uppercase shell variables such as `$HOME` are not skill invocations. Relative paths inside skills are resolved by the agent against the skill's directory, which is included with its instructions. File references remain paths for the agent to read, not automatic file attachments.

Skill files are limited to 128 KiB each and expanded skill prompts to 512 KiB. The expanded instructions are stored with the prompt, so resuming history does not reload or rerun past skills. Lua `before_prompt` hooks receive the expanded prompt. Library users attach discovered skills through `Session::with_skills`.

**Trust:** project skills are not automatically invoked, but explicitly invoking one lets its instructions guide the agent's local tools. Review skills before use. Discovery and loading do not execute scripts; scripts referenced by a skill run only if the agent chooses to call a tool.

## Lua configuration

The harness loads `~/.ri/config.lua` using `HOME`. XDG configuration/data paths are not used. A missing default file is fine. `--config PATH` loads an explicit file and reports missing-file or Lua errors. Project-local configuration is **never loaded automatically**.

Start with [examples/config.lua](examples/config.lua):

```sh
mkdir -p ~/.ri
cp examples/config.lua ~/.ri/config.lua
cargo run -p ri-agent-cli -- --config examples/config.lua
```

The file returns a table with:

- `settings`: `model`, `reasoning_effort`, `base_url`, `max_turns`, `command_timeout`, `max_output_bytes`.
- `hooks.before_prompt(text)`: transform each submitted user prompt.
- `hooks.after_response(text)`: transform assistant text before display and storage, including text accompanying tool calls. Configuring this hook buffers response text until the hook finishes, so untransformed text is never displayed.
- `tools`: an array of `{ name, description, parameters, execute }`. `parameters` is an object JSON schema; `execute(args)` receives decoded JSON arguments. Return a string or a JSON-serializable Lua value. Validate arguments inside the tool; schemas are advertised to the model, not enforced locally. Names cannot shadow built-ins or each other.

Hooks return a replacement string or `nil` to leave text unchanged. Hook errors fail the current turn. Tool errors are returned to the model for recovery. Custom tool results are capped by `max_output_bytes` before entering model history.

**Lua is trusted executable code, not a sandbox.** It can access local files, environment variables, and processes. Callbacks run on blocking threads, but have no execution timeout and cannot be forcibly cancelled; a blocked callback can delay process exit. `command_timeout` applies to built-in Bash, Search, and Find. Keep callbacks finite, avoid terminal writes (`print`, `io.write`, subprocess output) while using the TUI, and do not load untrusted configuration. No credentials belong in configuration files; use `OPENROUTER_API_KEY`.

### Options and precedence

CLI flags override environment variables (`MODEL`, `OPENROUTER_BASE_URL`), which override Lua settings, which override built-in defaults.

| Option | Default |
| --- | --- |
| `-p`, `--prompt` | No prompt opens the TUI |
| `--tui` | Force TUI, optionally with an initial `-p` prompt |
| `--config` | User config path described above |
| `--model` | OpenRouter: `anthropic/claude-haiku-4.5`; Codex: an advertised model from its live catalog |
| `--reasoning` | Provider default; accepts `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` after catalog capability validation |
| `--resume [ID]` | Resume the latest session when ID is omitted |
| `--sessions` | List saved sessions and exit |
| `--session-db` | User SQLite database path described above |
| `--no-save` | Disable persistence |
| `--base-url` | `https://openrouter.ai/api/v1` |
| `--max-turns` | `20` per submitted prompt |
| `--command-timeout` | `60` seconds per Bash call |
| `--max-output-bytes` | `32768` retained bytes per Read, Bash stream, or Lua tool result |

All numeric limits must be positive. Truncation adds an `[output truncated]` marker.

## Tools and security

- **Read**: read a file, subject to the output limit.
- **Write**: create or overwrite a file; the parent directory must exist.
- **Edit**: replace one unique exact `old_string` with `new_string` in an existing UTF-8 `file_path`. Read the file first and include surrounding text to disambiguate repeated matches. Empty, missing, ambiguous (including overlapping), and unchanged matches are rejected without writing. An empty replacement deletes the matched text. Returns a unified diff, capped by `max_output_bytes`; the TUI colors additions and deletions, including in resumed sessions. Output truncation does not truncate the file edit.
- **Search**: search file contents with `rg`, returning matching paths, line numbers, and text. Arguments: `pattern` (regular expression), optional `path` (defaults to `.`). No matches is a normal result.
- **Find**: discover files with `fd`. Arguments: `pattern` (filename regular expression; empty lists all files), optional `path` (defaults to `.`). Returns a JSON array of paths plus a truncation flag. Search and Find respect ignore files and skip hidden files by default; both apply `command_timeout` and `max_output_bytes` and invoke commands directly without shell interpolation.
- **Bash**: run a command in a fresh noninteractive shell, returning stdout, stderr, and exit status. Shell state does not persist.

Consecutive Read calls run concurrently (up to four). Other tools run sequentially. Tools operate with the process's working directory and permissions, **without approval prompts or filesystem sandboxing**. File contents and command output can be sent to the model provider.

## Workspace

```text
crates/agent/  ri-agent       Main library: sessions, protocol, events, built-in tools
crates/lua/    ri-agent-lua   Lua settings, hooks, custom-tool runtime
crates/tui/    ri-agent-tui   Ratatui interface using the main library
crates/cli/    ri-agent-cli   CLI configuration and interface selection
```

## Providers

The library exposes the `Provider` trait in `crates/agent/src/providers/mod.rs`. `OpenRouter` and `OpenAI Codex` implement model discovery, reasoning capabilities, and completions. Codex uses ChatGPT OAuth credentials only. Provider implementations live in `crates/agent/src/providers/` (`openrouter.rs` and `codex.rs`). They own their transport and error translation; sessions depend only on the trait. The canonical library namespace is `ri_agent::providers`; the original `ri_agent::provider`, `ri_agent::openrouter`, and `ri_agent::codex` paths remain compatibility re-exports. No non-OpenRouter endpoint is accepted unless an explicit `OPENROUTER_API_KEY` is provided for testing.

## Authentication security

Browser login uses S256 PKCE with a random verifier and no API key in the browser URL. OpenRouter uses a random loopback port; Codex uses port 1455 and a random state verified on callback. Codex manual login skips the listener and accepts the full callback URL through a hidden prompt. Callback listeners are bound to loopback and use a 120-second read timeout. `ri login --api-key` uses a hidden terminal prompt and validates the key before saving. Login errors avoid echoing keys or authorization codes. If a browser cannot open, the URL is printed for manual opening.

The root Cargo manifest is workspace-only. `crates/agent/src/providers/openrouter.rs` handles OpenRouter catalog capabilities; `crates/agent/src/sessions.rs` handles SQLite persistence.

The main library re-exports the Lua crate as `ri_agent::config`. Frontends use `Session` and an event channel (`Output::channel`); `Output::default` writes CLI output. Sessions own conversation history and expose `submit` and `clear`.

`Provider::complete_stream` emits provisional `Event::AssistantDelta` text and returns the complete response; its default implementation delegates to `complete` for existing providers. `Event::Assistant` is authoritative and replaces any preview; `Event::AssistantAborted` marks an unfinished preview. Only complete messages enter conversation history. The shared SSE reader handles fragmented UTF-8, CR/LF framing, multiline data, and keepalive comments, with an 8 MiB per-event limit. Missing completion markers and provider error events fail the turn without executing pending tools. Protocol references: [OpenAI streaming responses](https://developers.openai.com/api/docs/guides/streaming-responses) and [OpenRouter reasoning details](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens).

Library frontends can call `Session::submit_cancellable(prompt, &cancellation)` and retain a clone of `cancellation::Cancellation` to request a stop. Create a fresh signal for every turn; signals are never reset. The call returns `SubmitOutcome::Completed` or `SubmitOutcome::Cancelled` after saving, or an error if the turn/checkpoint fails. Existing `Session::submit` behavior is unchanged.

## Development

Inside `nix develop`:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests cover local tools, Lua validation/hooks/tools, TUI state/rendering, and mock-HTTP agent sessions. They need no live credentials and do not verify live-provider compatibility. `nix build` packages `result/bin/ri`; run `nix fmt` after editing Nix files.
