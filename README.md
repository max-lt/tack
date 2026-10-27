# ccrs

A lightweight reimplementation of [Claude Code](https://github.com/anthropics/claude-code) in Rust.

## Why?

`ccrs` is designed for performance and efficiency. While the official Claude Code CLI is a powerful tool, it requires approximately 175 MB of disk space and 500 MB – 3 GB+ of RAM during operation due to its Node.js/Bun architecture.

`ccrs` provides the same functionality in a **~23 MB** binary using **~10 MB RAM**, making it ideal for resource-constrained environments or users who prefer minimal overhead.

## Features

- **Full tool suite** — Bash, Git, Read, Write, Edit, Glob, Grep, Search
- **Terminals** — `TerminalOpen`, `TerminalSend`, `TerminalRead`, `TerminalClose` for programs that need a TTY
- **Monitors** — `Monitor` runs a command in the background, and its output wakes the model (see [Monitors](#monitors))
- **Sandbox** — `--sandbox <IMAGE>` runs commands in an offline Linux VM (Apple `container`, macOS 26 or later)
- **Crosstalk** — with `--features crosstalk`, talk to the agents of other people through a `crosstalk` daemon
- **Agentic loop** — tool_use → permission check → execute → send result → continue
- **Interactive permissions** — colored prompts with rule-based auto-allow
- **Smart Git integration** — read-only commands (status, log, diff) auto-approved, write operations require permission
- **Settings compatibility** — reads `.claude/settings.json` and `.claude/settings.local.json` (same format as Claude Code)
- **Two providers** — Anthropic, and Cerebras for open-weights models
- **OAuth PKCE** — same auth flow as the official CLI, with refresh token rotation
- **API key authentication**
- **Streaming responses** via SSE
- **Slash commands** — `/help`, `/quit`, `/clear`, `/model`

## Install

```
cargo install --path crates/cli
```

## Usage

```
ccrs
```

On first launch, authenticate via **OAuth** (browser) or **API key** (`sk-ant-...`).

Then type your messages at the `>` prompt. Claude streams responses and uses tools as needed, asking permission before executing.

`ANTHROPIC_API_KEY`, when set, takes priority over the saved credentials.

| Flag | Description |
|------|-------------|
| `-p, --print [PROMPT]` | Answer one prompt without the TUI. The reply goes to stdout. With no prompt, `ccrs` reads it from stdin. Tools that no rule allows are refused. |
| `--session <ID>` | Resume the session with this id, or start a session with this id. |
| `-r, --resume [ID]` | Resume a saved session. With no id, pick one from this directory. |
| `-c, --continue` | Resume the most recent session in this directory. |
| `--sandbox <IMAGE>` | Run commands in an offline Linux VM from this OCI image. |
| `--sandbox-allow <HOSTS>` | Hosts that the sandbox can reach over HTTPS, for example `crates.io,*.crates.io`. |

```
ccrs -p "Summarize the last commit" --session review
echo "And the commit before it?" | ccrs -p --session review
```

### Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `/help` | `/h` | Show available commands |
| `/quit` | `/q` `/exit` | Exit |
| `/clear` | | Clear conversation history |
| `/model` | | Pick a model with ↑/↓, Enter to confirm, Esc to cancel |
| `/model <name>` | | Switch model (e.g. `/model opus`) |
| `/rec` | | Record and transcribe voice input (requires `MISTRAL_API_KEY`) |

### Models

`/model <name>` selects the model. The provider follows from the model id.

| Model | Provider | Context | Credential |
|-------|----------|---------|------------|
| `claude-sonnet-5-5` | Anthropic | 200k | OAuth or API key |
| `claude-opus-5-5` | Anthropic | 200k | OAuth or API key |
| `claude-haiku-5-5` | Anthropic | 200k | OAuth or API key |
| `qwen-3.8-27b` | Cerebras | 64k | `CEREBRAS_API_KEY` |
| `gpt-oss-120b` | Cerebras | 65k | `CEREBRAS_API_KEY` |

Cerebras context figures are for the free tier. The paid tier gives twice as much.

To use a Cerebras model, put the key in `.env` at the project root:

```
CEREBRAS_API_KEY=csk-...
```

`ccrs` reads `.env` at startup. An exported variable takes priority.

Cerebras speaks the OpenAI chat completions format. `ccrs` translates each
request and each response stream. Tool calling works on both providers.

### Voice Input

The `/rec` command uses [Mistral AI's Voxtral](https://mistral.ai/) for transcription:

1. Set `MISTRAL_API_KEY` environment variable
2. Type `/rec` in the CLI
3. Speak into your microphone
4. Press Enter to stop recording
5. Edit the transcription if needed
6. Press Enter to send the message

```bash
export MISTRAL_API_KEY=your_api_key_here
ccrs
```

**Note:** Voice recording temporarily exits the TUI to access the microphone. You'll return to the TUI after transcription.

### Permissions

Create `.claude/settings.local.json` in your project:

```json
{
  "permissions": {
    "allow": [
      "Bash(cargo:*)", 
      "Bash(git:*)",
      "Git(commit:*)",
      "Git(push:*)"
    ],
    "deny": ["Bash(rm -rf:*)"],
    "additionalDirectories": ["/path/to/other/project"]
  }
}
```

**Auto-approved tools:**
- `Glob`, `Grep`, `Search`, `List` — always allowed
- `TerminalRead`, `TerminalClose`, `MonitorList`, `MonitorStop`, `CrosstalkWhoami`, `CrosstalkPeers` — always allowed
- `Read`, `Write`, `Edit` — auto-allowed in project directory
- `Git status`, `Git log`, `Git diff`, `Git show`, `Git blame`, `Git branch` — read-only git commands

**Require permission:**
- `Bash` commands (unless explicitly allowed)
- `TerminalOpen`, `TerminalSend`, `Monitor` — rules: `TerminalOpen(cmd:*)`, `TerminalSend(...)`, `Monitor(cmd:*)`
- A `Bash(...)` deny rule also denies that command in `Monitor` and `TerminalOpen`.
- No allow rule covers `Monitor` or `TerminalOpen` with `host: true`. A run on the host always asks, except with `--auto` or `--dangerously-skip-permissions`.
- `CrosstalkSend`, `CrosstalkConnect`, `CrosstalkForget` — the data leaves the machine
- `Git commit`, `Git push`, `Git reset`, `Git checkout`, `Git add`, etc. — write operations

Three layers, merged in order:

1. `~/.claude/settings.json` — global
2. `.claude/settings.json` — project (committed)
3. `.claude/settings.local.json` — local (gitignored)

**Flags:**
- `--auto` — approve tools without prompting. `deny` rules still apply.
- `--dangerously-skip-permissions` — approve **every** tool call, ignoring all rules including `deny`. Use only in a sandbox (container, VM).

### Monitors

The `Monitor` tool runs a command in the background. Each line that the command writes to stdout or stderr wakes the model. `MonitorList` lists the monitors. `MonitorStop` stops one.

| Limit | Value | When the limit is reached |
|-------|-------|---------------------------|
| Monitors that run at one time, per session | 8 | `Monitor` returns an error. At resume, the note lists the monitors that did not start. |
| Events per minute, per monitor | 10 | The monitor stops. Its last event gives the reason to the model. |
| Events per minute, all monitors of a session | 20 | The monitor that goes over the limit stops. Its last lines are not sent. |
| Lines in one event | 100 | The event tells how many lines it dropped. |
| Bytes in one line | 1000 | The line is cut and ends with an ellipsis (U+2026). |

Lines that come less than 300 ms apart go into one event. An event stays open for 2 s or less.

Output:

- The model gets the output as quoted data, not as an instruction.
- Bytes that are not UTF-8 become U+FFFD.
- Control characters, except tab, become `\xNN` text. An escape code in a log does not change the terminal.

Permissions and resume:

- `Monitor` asks for permission, like `Bash`. A rule such as `Monitor(tail:*)` allows it. `MonitorList` and `MonitorStop` do not ask.
- The session file keeps the monitors. A resumed session asks again before it starts each one, because the file can change between runs.
- `ccrs -p` does not start the saved monitors.

Stop:

- A stop, the end of the command, the exit of `ccrs`, SIGTERM and SIGHUP kill the process group of the command.
- After SIGTERM or SIGHUP, `ccrs` exits with code 143 or 129.

Known limits:

- A process that leaves the process group (`setsid`, a daemon) does not stop with the monitor. The model gets this warning.
- SIGKILL on `ccrs` does not stop the monitors.
- In the sandbox, a stop kills the `container exec` client. A process in the VM can run until the VM stops, 5 minutes after the last command.

## Architecture

```
crates/
  core/   API client, streaming, tools (Bash/Git/Read/Write/Edit/Glob/Grep/Search), permissions, auth
  cli/    Terminal UI, interactive permissions, slash commands
  search/ Tantivy-based semantic search
```

## Credentials

Anthropic credentials go in `~/.config/claude-code-rs/credentials.json` (mode `0600`).
Delete the file to re-authenticate.

Provider keys come from the environment or from `.env`. Keep `.env` out of git.
