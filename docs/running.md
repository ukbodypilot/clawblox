---
title: Running worlds
summary: "Run a world locally, from GitHub, with recording, or resumed from a snapshot."
read_when:
  - Running a world
  - You need run flags like port, record, or resume
---

Every world runs the same way: point `clawblox run` at a folder that contains a
[`world.toml`](/worlds). This page covers the flags you add on top of that. To
start an agent alongside the world, see [Agents](/agents).

## Single world

```bash
clawblox run <world>
```

`<world>` can be a local folder or a GitHub folder URL. The folder must contain
`world.toml`. Run it from inside the folder to use the current directory:

```bash
cd my-world
clawblox run
```

Either way, this starts a local server on `http://localhost:8080`.

Common flags:

```bash
clawblox run <world> --port 8280
clawblox run <world> --agent codex          # also start an agent
clawblox run <world> --record --record-dir runs/r001/recordings
clawblox run <world> --resume runs/r001/resume/latest.json
```

Runs that start an agent with `--agent` record by default (to `<world>/recordings`
unless `--record-dir` is set); pass `--no-record` to turn that off.

`--agent` starts a managed agent in the world; see [Agents](/agents)
for coding agents, sandboxing, and multi-agent runs. `--resume` restores a saved
world snapshot; see [Save, load, and resume](/save-load-resume) for what a
snapshot captures.

## Remote worlds

GitHub folder URLs are downloaded into the Clawblox home directory, then run as
ordinary local worlds.

Supported URL shape:

```text
https://github.com/<owner>/<repo>/tree/<ref>/<path-to-world>
```

Example:

```bash
clawblox run https://github.com/nacloos/clawblox/tree/main/worlds/mesa-island
```

The downloaded folder is cached under `~/.clawblox/worlds/`. Set `CLAWBLOX_HOME`
to use another location:

```bash
CLAWBLOX_HOME=/tmp/clawblox clawblox run https://github.com/nacloos/clawblox/tree/main/worlds/mesa-island
```

## Resume

Restore a saved snapshot with `--resume`:

```bash
clawblox run <world> --resume <snapshot.json>
```

Clawblox restores that exact snapshot or exits with an error. A missing,
unreadable, or incompatible snapshot stops the run; it never silently starts a
fresh world instead. Relative paths are resolved from the world directory.

Replay and resume are different things: replay plays back a recording of what
happened, resume restores a live world to continue from. See
[Save, load, and resume](/save-load-resume) for what a snapshot contains, and
[Checkpoints](/checkpoints) for resuming a world together with its agents.

## Notes

- Restart the server after editing world files.
- Use `clawblox start` to run in the background and `clawblox stop` to stop it.
- For multi-agent runs and agent sandboxing, see [Agents](/agents).
