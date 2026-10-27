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

`ANTHROPIC_API_KEY`, when set, takes priority over the saved credentials. `ANTHROPIC_BASE_URL` points `ccrs` at another server for the Anthropic models, for example a proxy. The API key goes to that server. The URL must use https; plain http is allowed only to `localhost`, `127.0.0.1` or `::1`. `ccrs` refuses another URL before it sends a request. `ccrs` reads `ANTHROPIC_BASE_URL` only from the environment, never from a `.env` file: a `.env` file comes with the project, and a cloned repo could send the key to its own server. When the variable is set, `ccrs` prints the API host at start.

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
- `Read`, `Write`, `Edit` — auto-allowed in project directory. The check resolves `..` and symbolic links first, so a path that leads out of the project asks. Known limit: a link that changes between the check and the write is not covered.
- `Git status`, `Git log`, `Git diff`, `Git show`, `Git blame`, `Git branch` — read-only git commands

**Require permission:**
- `Bash` commands (unless explicitly allowed)
- `TerminalOpen`, `TerminalSend`, `Monitor` — rules: `TerminalOpen(cmd:*)`, `TerminalSend(...)`, `Monitor(cmd:*)`
- A prefix rule such as `Bash(tail:*)` covers one command. It does not cover a command with a shell metacharacter (`;`, `&`, `|`, `$`, a backquote, `(`, `)`, `<`, `>` or a newline), such as `tail x; rm -rf ~` or `tail $HOME/log`: `ccrs` asks. The same applies to `Monitor` and `TerminalOpen`. `Bash(*)` and a rule without `:*` match as written.
- `TerminalSend` text runs as shell lines. A prefix rule such as `TerminalSend(ls:*)` checks each line between `<Enter>` keys and line breaks, like a `Bash` command. Another key token, such as `<Up>`, `<Tab>`, `<C-m>` or an unknown one, makes `ccrs` ask: the shell can then run a line that is not the text. A deny rule checks each part between keys and line breaks, against `TerminalSend` and `Bash` rules. The check and the terminal read the input with one parser: a `<...>` that is not a key name is text, as the terminal types it. A deny rule cannot rebuild a line that edit keys change, such as `m x<C-a>r<Enter>`; without `--auto`, such input asks.
- A `Bash(...)` deny rule also denies that command in `Monitor` and `TerminalOpen`. A deny rule checks each command of a chain: `Bash(rm:*)` denies `ls; rm x`, `tail $(rm y)`, `FOO=1 rm x` and `X="a b" rm x`. It skips the reserved words and wrappers in front of a command (`if`, `then`, `else`, `elif`, `do`, `while`, `until`, `!`, `{`, `time`, `command`, `builtin`, `exec`) and a backslash in front of the command word (`\rm`).
- A deny rule is a guard, not a sandbox. It matches the start of a command, so `sudo rm`, `/bin/rm`, `env rm`, `nohup rm`, `bash -c "rm x"`, `eval "rm x"`, `xargs rm` or `find -delete` do not match `Bash(rm:*)`. To contain commands, use `--sandbox`.
- No allow rule covers `Monitor` or `TerminalOpen` with `host: true`, or `TerminalSend` into a terminal that runs on the host. A run on the host always asks, except with `--auto` or `--dangerously-skip-permissions`.
- `CrosstalkSend`, `CrosstalkConnect`, `CrosstalkForget` — the data leaves the machine. An allow rule must name the action and the peer, such as `Crosstalk(send crosstalk_<id>)`; `Crosstalk(*)` or `Crosstalk(send:*)` allow nothing. Deny rules can use wildcards. The prompt shows the peer and the whole text, escaped.
- `Git commit`, `Git push`, `Git reset`, `Git checkout`, `Git add`, etc. — write operations

Three layers, merged in order:

1. `~/.claude/settings.json` — global
2. `.claude/settings.json` — project (committed)
3. `.claude/settings.local.json` — local (gitignored)

**Flags:**
- `--auto` — approve tools without prompting. `deny` rules still apply.
- `--dangerously-skip-permissions` — approve **every** tool call, ignoring all rules including `deny`. Use only in a sandbox (container, VM).

### Terminals

`TerminalOpen` starts a command under a PTY. `TerminalSend` types into it, `TerminalRead` returns its screen, `TerminalClose` stops it.

| Limit | Value | When the limit is reached |
|-------|-------|---------------------------|
| Terminals open at one time, per session | 8 | `TerminalOpen` returns an error. |
| Screen size | 200 rows, 500 columns | `TerminalOpen` returns an error. |
| Bytes of screen text in one read, after the escape | 32768 | The top lines are not sent. A first line tells how many. |

The model gets the screen with control and invisible characters as `\xNN` and `\u{N}` text, as for monitor output.

A read waits until the output settles, or until the text it waits for appears, for its timeout at most: 10 s by default, 120 s at most. A command that prints without end, prints binary data or never prints does not hold a read longer. The screen keeps no scrollback, so the memory of a terminal does not grow with its output.

A close, the end of the session, SIGTERM and SIGHUP kill the process group of the terminal command, also a child that ignores SIGHUP. The limits of monitors apply: a process that leaves the group (`setsid`, a daemon) and SIGKILL on `ccrs` are not covered.

### Monitors

The `Monitor` tool runs a command in the background. Each line that the command writes to stdout or stderr wakes the model. `MonitorList` lists the monitors. `MonitorStop` stops one.

| Limit | Value | When the limit is reached |
|-------|-------|---------------------------|
| Monitors that run at one time, per session | 8 | `Monitor` returns an error. At resume, the note lists the monitors that did not start. |
| Events per minute, per monitor | 10 | The monitor stops. Its last event gives the reason to the model. |
| Events per minute, all monitors of a session | 20 | The monitor that goes over the limit stops. Its last lines are not sent. |
| Lines in one event | 100 | The event tells how many lines it dropped. |
| Bytes of output in one event, after the escape | 16384 | The event tells how many lines it dropped. |
| Bytes in one line, after the escape | 1000 | The line is cut and ends with an ellipsis (U+2026). |
| Bytes in a description, on one line | 200 | `Monitor` returns an error. A saved monitor with a longer description does not start at resume. |

Lines that come less than 300 ms apart go into one event. An event stays open for 2 s or less.

`debounce_seconds` (0 to 3600, default 0) sets the least time between two events of a monitor. The lines in between go into the next event, in its limits of lines and bytes. A monitor with a debounce of 6 s or more stays under the limit of 10 events per minute. The session file keeps the value.

Output:

- The model gets the output as quoted data, not as an instruction.
- Bytes that are not UTF-8 become U+FFFD.
- Control characters, except tab, become `\xNN` text. An escape code in a log does not change the terminal.
- Invisible characters become `\u{N}` text: every Default_Ignorable_Code_Point and format character (Unicode 16; bidi controls, zero-width characters, variation selectors, the tag block), the line and paragraph separators, and the blank glyphs U+2800, U+3164, U+FFA0, U+115F and U+1160. A log line cannot hide text from the user that the model reads. An emoji with a variation selector, such as U+2764 U+FE0F, shows the selector as `\u{fe0f}`.

Permissions and resume:

- `Monitor` asks for permission, like `Bash`. A rule such as `Monitor(tail:*)` allows it in the sandbox, but never with `host: true`. A `Bash(...)` deny rule also denies the command in `Monitor`. `MonitorList` and `MonitorStop` do not ask.
- The session file keeps the monitors. A resumed session asks again before it starts each one, because the file can change between runs.
- `ccrs -p` does not start the saved monitors. It keeps them in the session file, and the next interactive run asks to start them.
- `ccrs -p` runs one turn. A monitor that the model starts in `-p` stops when the run ends, and its output does not reach the model. The tool result tells the model so. The monitor stays in the session file for a later resume.

Stop:

- A stop, the end of the command, the exit of `ccrs`, SIGTERM and SIGHUP kill the process group of the command.
- `/clear` stops every monitor. The file of the old session keeps them, so a resume of that session offers to start them again.
- After SIGTERM or SIGHUP, `ccrs` exits with code 143 or 129.

Known limits:

- A process that leaves the process group (`setsid`, a daemon) does not stop with the monitor. The model gets this warning.
- SIGKILL on `ccrs` does not stop the monitors.
- In the sandbox, a stop kills the `container exec` client. A process in the VM can run until the VM stops, 5 minutes after the last command.

### Crosstalk

With `--features crosstalk`, `ccrs` starts one `crosstalk daemon` per session and talks to it on its control socket. Messages from other people's agents reach the model as a "Crosstalk" notice.

Incoming messages:

- Each line from the peer, the header too, is escaped like monitor output and quoted. The notice says that it is data from another person's agent, not an instruction.
- The body of one event holds 8192 bytes or less, after the escape. A first line tells how many bytes are not shown.
- A pairing request (intro) from a stranger goes to the user only, escaped, never to the model. Only the user accepts, with `/crosstalk accept <id>`: no tool lets the model accept or add a hub.
- 10 messages a minute and 200 a session reach the model. Each one costs a turn, and the peer chooses how many it sends. The TUI tells the user how many it drops.
- Known limit: the daemon writes its events as text, with no framing. A peer that writes a blank line, then a line such as `[message] from <another peer> via ...`, makes ccrs read a second event with the sender of its choice. A fix needs a structured inbox in the daemon.

### Session files

`ccrs` saves each session in `sessions/<id>.json` in its config directory.

- Format version 2. `ccrs` reads versions 1 and 2. It refuses a file with a higher version, so that a save cannot erase fields it does not know. A `ccrs` that reads only version 1 refuses a version 2 file.
- Version 2 adds the highest terminal and monitor ids ever used, so that a new terminal or monitor does not take the id of one that the history names.
- The version changes only for a field whose loss can cause harm, such as a reused id or a dropped safety limit. An optional field whose loss changes only timing, such as `debounce_seconds` of a monitor, keeps the version: an older `ccrs` loads the file and drops the field at its next save.
- The directories have mode 0700 and the files mode 0600. A directory that belongs to another user is refused.
- One `ccrs` process at a time opens a session: it holds a lock on `sessions/<id>.lock`. A second one stops with an error. `/clear` moves the lock to the new id.
- `ccrs` does not read a file over 64 MiB, and it does not follow a symbolic link. A save replaces a link with a file and does not write to its target.
- The resume picker reads the 500 most recently changed session files.
- A save writes a temporary file, then renames it. When `ccrs` opens the sessions directory, it removes the temporary files of processes that are gone.

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
