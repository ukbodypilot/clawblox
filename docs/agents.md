---
title: Agents
summary: "Run coding agents as embodied agents in worlds, and where their state lives."
read_when:
  - Running an agent in a world
  - Choosing a coding agent or inspecting agent state
---

Clawblox runs long-lived agents in interactive worlds. The agents are existing
coding agents such as Codex and Claude Code; Clawblox handles the world
connection, workspace, session files, logs, and resume and checkpoint plumbing
around them.

An agent has a name, a workspace, session files, transcripts, logs, and a
connection to a world. Clawblox keeps those pieces together so runs are easy to
inspect, continue, checkpoint, and compare. Agents are not tied to the Clawblox
engine: they can run in any world that implements the [world protocol](/worlds).

## Coding agents

A coding agent is a tool like Codex, Claude Code, or Kimi: a CLI that already
knows how to talk to its model provider. Clawblox starts and manages it; it does
not replace it.

Supported coding agents:

```text
codex     Codex CLI
claude    Claude Code
kimi      Kimi Code
```

Install and log in to the coding agent you want before running it. You also need
`tmux`: Clawblox starts the coding agent inside a tmux session so it keeps
running and can be inspected.

## Running an agent

The quickest way is to pass `--agent` when starting a world:

```bash
clawblox run <world> --agent codex
```

This starts the world, then starts a managed agent connected to it. Useful
options:

```bash
clawblox run <world> \
  --agent codex \
  --agent-name Rocky \
  --model gpt-5.5 \
  --agent-dir runs/rocky \
  --sandbox
```

Use Python to orchestrate worlds and agents from a script:

```python
from pathlib import Path
from clawblox import Agent, World, default_system_prompt

world = World(dir="worlds/mesa-island")
world.start(port=8080, record=True, record_dir=Path("runs/r001/recordings"))

agent = Agent(
    agent="codex",          # which coding agent
    name="Rocky",           # this agent's name
    dir=Path("runs/r001/agents/rocky"),
    model="gpt-5.5",
    sandbox=True,
)

access = world.connect(agent=agent)
agent.start(
    initial_prompt="Begin.",
    system_prompt=default_system_prompt(),
)
```

`world.connect(agent=agent)` joins the world, writes the world session id into
the agent directory, and sets the environment the agent needs. The agent then
reads the world's [protocol](/worlds#the-world-protocol) to learn how to observe
and act.

## Running on a Claude subscription

A coding agent left to run nonstop spends a Claude Pro or Max plan's usage
limit in an hour or two, then stalls until the limit resets. To keep a Claude
agent playing around the clock instead, give it a request budget:

```bash
clawblox run <world> --agent claude --plan pro
```

`--plan pro|max5|max20` sets a budget of model requests per 5-hour window and
per week, and spreads them evenly so the agent never runs dry. `--plan pro`
also defaults the model to Sonnet. Anthropic does not publish exact limits, so
the plan budgets are rough, conservative estimates (Pro: 250 per 5 hours and
3000 per week, about one request every 3.4 minutes). Tune them with
`--rate-limit`, which replaces the plan's budget and can be repeated:

```bash
clawblox run <world> --agent claude --model sonnet \
  --rate-limit 300/5h --rate-limit 4000/week
```

| Option | Meaning |
| --- | --- |
| `--rate-limit N/PERIOD` | At most `N` model requests per window. Periods: `s`, `min`, `h`, `day`, `week`, with an optional multiplier such as `5h`. |
| `--min-request-interval SECONDS` | Minimum gap between model requests. |
| `--burst` | Spend each window as fast as allowed instead of evenly, then wait for it to free up. |

The agent's `PreToolUse` and `Stop` hooks enforce the budget: each one marks a
finished model response and sleeps until the next request fits. Usage is kept
in `runtime/pace_state.json`, so restarts keep counting, and every pause is
logged to `logs/pace.log`. Check real usage with `/status` inside the agent's
tmux session and adjust. Keep the even interval under about 5 minutes so
Claude Code's prompt cache stays warm between requests.

To change the budget while the agent runs, use `clawblox pace` with the
agent's directory (by default `<world>/.clawblox/agents/<name>`):

```bash
clawblox pace worlds/mesa-island/.clawblox/agents/Rocky --rate-limit 300/5h --rate-limit 4500/week
clawblox pace worlds/mesa-island/.clawblox/agents/Rocky          # show the live budget
clawblox pace worlds/mesa-island/.clawblox/agents/Rocky --reset  # back to the startup budget
```

It writes `runtime/pace.json`, which the hooks re-read every few seconds,
including during a pause, so the new budget applies right away. It also accepts
`--min-request-interval`, `--burst` and `--even`.

If the agent still hits a usage limit, the recovery watchdog waits
`CLAUDE_RATE_LIMIT_BACKOFF_SECONDS` (default 1800) and restarts it, so the run
resumes on its own once the limit resets.

## Sandbox

Pass `--sandbox` (CLI) or `sandbox=True` (Python) to run the coding agent in a
restricted sandbox. A world can declare its sandbox policy in `world.toml`:

```toml
[sandbox]
deps_root = "/opt/conda"
enable_weaker_nested_sandbox = true
add_deps_lib_to_library_path = true
```

`deps_root` is mounted read-only at `/sandbox-deps` for sandboxed agents.
Absolute dependency prefixes are also mounted read-only at their original path,
so console scripts whose `#!` line points to an absolute interpreter path keep
working. When
`add_deps_lib_to_library_path` is true, Clawblox adds `/sandbox-deps/lib` to
`LD_LIBRARY_PATH`; disable it if the prefix has libraries that conflict with
system tools. `enable_weaker_nested_sandbox` is only for unprivileged
Docker/WSL2 containers where standard `bubblewrap` sandboxing is unavailable;
leave it unset on normal hosts.

## Multiple agents

Use the Python `World` and `Agent` APIs above to connect multiple agents, each
with its own name and directory, to a running world. Set `max_players` in the
world's `world.toml` to accommodate them; the bundled Mesa world defaults to
one player. See [Checkpoints](/checkpoints) for saving and resuming world and
agent state together.

## Workspace and memory

Each agent has an agent directory that keeps everything needed to inspect and
continue it:

```text
agent-dir/
  workspace/         persistent working area and memory files
  runtime/           generated system prompt and rerunnable command script
  logs/              agent and runtime logs
  session/           provider continuation state and transcripts
  home/              agent home/config directory
  events.jsonl       structured operation log
  world_session.txt  world session id
```

`workspace/` is the agent's persistent working area. Memory files live there,
along with anything the agent creates while acting in the world. The default
prompt expects files such as:

```text
SOUL.md
SEMANTIC_MEMORY.md
EPISODIC_MEMORY.md
```

World-specific templates can seed these files. `session/` stores provider
continuation state and transcripts when the coding agent supports them.

The workspace does not depend on which coding agent you used, which makes it the
safest part of an agent's state to share. See [Checkpoints](/checkpoints) to package world and agent state together,
and [Agent integration](/agent-interface) to add support for a new coding agent.
