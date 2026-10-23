# Running Clawblox on Windows (instructions for a Claude Code agent)

This file is written for a Claude Code instance running on a Windows machine
that has been asked to set up Clawblox and get an AI agent playing in a world.
Follow it top to bottom. Steps marked **USER** need the human at the keyboard
(UAC prompts, reboots, browser logins); stop and ask them when you reach one.

## TL;DR

- The Clawblox **world server** runs natively on Windows (there is a Windows
  installer), but the **managed agent launcher** (`clawblox run <world> --agent claude`)
  does not. It is hard-wired to Unix tooling.
- The supported path is **WSL2 (Ubuntu)**: install Clawblox, tmux, python3 and
  a second copy of Claude Code *inside* WSL, and run everything there. The
  browser viewer at `http://localhost:8080` still opens from Windows.
- A native-only fallback exists (Option B below) but it loses the automatic
  agent loop, memory hooks and logging.

## Why tmux is a hard requirement (what the code does)

When you pass `--agent`, `src/agent_runner.rs::start_agent` does the following:

1. Writes a launcher script `<agent-dir>/runtime/start.sh` and runs it with
   `bash`.
2. Starts it inside a detached tmux session named
   `clawblox-<kind>-<agent-name>` (default `clawblox-claude-agent`) via
   `tmux new-session`, then uses `tmux send-keys` / `tmux capture-pane` to
   inject prompts and watch the pane (`start_tmux_target`, `tmux_send_literal`,
   and the recovery watchdog in `src/agent_recovery.rs`).
3. For `claude`, it generates a Claude Code plugin whose hooks are
   `#!/usr/bin/env bash` scripts and a `#!/usr/bin/env python3` script
   (`CLAUDE_STOP_SH`, `CLAUDE_POST_TOOL_USE_CHAT_PY` in `src/agent.rs`). The
   Stop hook is what keeps the agent playing forever ("Please don't stop...").
4. `--sandbox` additionally needs `bwrap` (bubblewrap) and `socat`, which are
   Linux-only.

There is no flag to skip tmux, and none of tmux, bash hooks, or bubblewrap
exist on native Windows, so swapping tmux for something like `psmux` would not
be enough. WSL2 gives you all of them unchanged.

---

## Option A (recommended): WSL2

### A1. Check / install WSL2

In PowerShell:

```powershell
wsl --status
wsl -l -v
```

If an Ubuntu distro is listed with `VERSION 2`, skip to A2.

Otherwise (**USER**: needs an elevated PowerShell and probably a reboot):

```powershell
wsl --install -d Ubuntu
```

After the reboot the user must open "Ubuntu" from the Start menu once to create
a Linux username and password. Then confirm with `wsl -l -v` that it is
version 2 (`wsl --set-version Ubuntu 2` if not).

### A2. How to run commands inside WSL from Windows

From your Windows-side shell, run Linux commands like this:

```powershell
wsl -d Ubuntu -- bash -lc "<command>"
```

Use `bash -lc` (login shell) so `~/.local/bin` is on `PATH`. Commands that
need `sudo` will prompt for the Linux password: either ask the user to run them
in an Ubuntu terminal, or ask them for permission before using
`sudo -S`. Never store the password in a file.

Work in the Linux filesystem (`~/...`), **not** under `/mnt/c/...`. File
watching and performance on `/mnt/c` are poor, and Unix permissions on the
generated hook scripts may not stick.

### A3. Install system packages

```bash
sudo apt-get update
sudo apt-get install -y tmux python3 curl git ca-certificates
```

Optional, only if you want `--sandbox` later:

```bash
sudo apt-get install -y bubblewrap socat
```

### A4. Install Clawblox inside WSL

```bash
curl -fsSL https://clawblox.com/install.sh | sh
```

This puts the binary in `~/.local/bin`. Verify:

```bash
bash -lc "clawblox --version"
```

If `clawblox` is not found, add `export PATH="$HOME/.local/bin:$PATH"` to
`~/.bashrc`.

Alternatively, build from this fork (needs Rust: `curl https://sh.rustup.rs -sSf | sh`,
plus `sudo apt-get install -y build-essential pkg-config libssl-dev`):

```bash
git clone https://github.com/ukbodypilot/clawblox.git ~/clawblox
cd ~/clawblox
cargo install --path .
```

Either way, clone the repo so you have the example world locally:

```bash
git clone https://github.com/ukbodypilot/clawblox.git ~/clawblox   # if not already
```

### A5. Install Claude Code inside WSL

The Windows copy of Claude Code cannot be used: Clawblox needs a `claude`
binary on the Linux `PATH`. Install the Linux build:

```bash
curl -fsSL https://claude.ai/install.sh | bash
bash -lc "claude --version"
```

Then authenticate it. **USER**: this is interactive. Ask the user to open an
Ubuntu terminal and run `claude` once, complete the login in the browser, and
accept any first-run prompts (theme, trust folder). Then `/exit`.

API-key alternative (no browser): put `export ANTHROPIC_API_KEY=...` in
`~/.bashrc` inside WSL. Only do this if the user gives you the key and agrees.

Note: Clawblox launches Claude with `--permission-mode bypassPermissions` by
default. If Claude Code shows a one-time "bypass permissions" confirmation, the
user must accept it (see A7 on attaching to the session).

### A6. Smoke test the world without an agent

```bash
cd ~/clawblox
clawblox run worlds/mesa-island
```

From Windows, open `http://localhost:8080` in a browser. WSL2 forwards
localhost by default. If the page does not load, get the WSL IP with
`wsl -d Ubuntu -- hostname -I` and use `http://<that-ip>:8080`.

Stop it with Ctrl+C. When you run this from your Windows shell through
`wsl ... bash -lc`, run it as a background task or it will block your session.
A simpler way is to use the daemon commands:

```bash
clawblox start worlds/mesa-island   # background
clawblox stop
```

### A7. Run the world with a Claude agent

```bash
cd ~/clawblox
clawblox run worlds/mesa-island --agent claude --agent-name Rocky
```

Useful flags (from `src/bin/cli.rs`):

| Flag | Default | Notes |
|------|---------|-------|
| `--agent claude` | | also `codex`, `kimi` |
| `--agent-name` | `agent` | becomes the tmux session suffix |
| `--model` | `claude-opus-4-6` | pass a model the user's account can use |
| `--agent-dir` | `worlds/mesa-island/.clawblox/agents/<name>` | workspace, logs, transcripts |
| `--tmux-session` | `clawblox-claude-<name>` | |
| `--permission-mode` | `bypassPermissions` | |
| `--sandbox` | off | needs bubblewrap + socat; see A9 |
| `-p / --port` | `8080` | |

`clawblox run` stays in the foreground (it hosts the world). Run it as a
background task from your Windows shell, or ask the user to run it in an
Ubuntu terminal.

Watch the agent:

- Browser: `http://localhost:8080`
- Attach to the agent's terminal (user, from Windows Terminal):
  `wsl -d Ubuntu -- tmux attach -t clawblox-claude-Rocky`
  (detach with `Ctrl+b` then `d`; do not Ctrl+C, that kills the agent).
- Peek without attaching (you can do this):
  `wsl -d Ubuntu -- tmux capture-pane -p -t clawblox-claude-Rocky`
- Logs: `<agent-dir>/logs/agent.log`, `<agent-dir>/events.jsonl`

### A8. Verify it is working

1. `tmux ls` inside WSL lists `clawblox-claude-<name>`.
2. `capture-pane` output shows Claude Code running and making `curl` calls to
   the world.
3. The browser shows a character moving.
4. `events.jsonl` has an `agent.start` entry with status other than `failed`.

If the session exits immediately, read `<agent-dir>/logs/agent.log` and the
`error` field in `events.jsonl`. Common causes: `claude` not logged in inside
WSL, `claude` not on PATH for non-login shells (symlink it:
`sudo ln -s ~/.local/bin/claude /usr/local/bin/claude`), or a model name the
account cannot use (pass `--model`).

If you see `tmux session '...' already exists`, kill it with
`tmux kill-session -t <name>` or pick another `--agent-name`.

### A9. Sandbox mode (optional)

`--sandbox` wraps the agent in bubblewrap. WSL2 sometimes cannot create the
needed user namespaces. If `--sandbox` fails, add this to the world's
`world.toml` (see `docs/agents.md`):

```toml
[sandbox]
enable_weaker_nested_sandbox = true
```

Skip the sandbox for the first run; get the plain agent working first.

---

## Option B (fallback): native Windows, no tmux

Use this only if WSL2 is not possible (for example, virtualization is disabled
and cannot be enabled).

1. Install Clawblox natively in PowerShell:
   ```powershell
   irm https://clawblox.com/install.ps1 | iex
   clawblox --version
   ```
2. Clone the repo and start a world **without** `--agent`:
   ```powershell
   git clone https://github.com/ukbodypilot/clawblox.git
   cd clawblox
   clawblox run worlds/mesa-island
   ```
3. In a separate Claude Code session, give it a prompt such as:
   > You are a being living in a world. The world base URL is
   > http://localhost:8080. Read http://localhost:8080/api.md, then
   > `POST /join?name=Rocky`, and loop on `GET /observe` / `POST /input`
   > as described there. Keep memory notes in this folder.

What you lose compared to Option A: no automatic "keep going" Stop hook (the
session ends when Claude ends its turn unless you keep prompting it), no
managed workspace/memory layout, no transcript collection, no recovery
watchdog, and no sandbox. The world protocol itself is just HTTP, so this works
for experimenting; see `docs/worlds.md` for the protocol.

---

## Report back to the user

When done, tell the user:

- which option you used,
- the exact command to start a world with an agent,
- the URL to watch (`http://localhost:8080`),
- the command to attach to the agent terminal,
- where the agent's workspace and logs are.
