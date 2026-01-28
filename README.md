# so

A sandbox orchestrator for your agents.

## Why so?

Run coding agents in isolated sandboxes so they can't break your host. Changes stay contained until you review and merge them back into your codebase.

## Features

- Isolated sandbox per run with bwrap or docker
- Multi-agent support: claude, opencode, codex
- Interactive review menu: diff, shell, reset, merge

## Installation

```bash
cargo install so
```

### Requirements

**Bubblewrap** (recommended):

```bash
sudo apt install bubblewrap
```

**Docker**:

macOS:

```bash
brew install --cask docker
```

Linux:

- Refer to the install guide from [Docker](https://docs.docker.com/engine/install/)

## Usage

Create a plan:

```bash
so plan
```

Edit `specs/*.md` for any changes.

Run the agent in a sandbox (default: docker):

```bash
so run
```

Choose a harness and iterations:

```bash
so run claude 5
so run codex 3
so run opencode 2
```

Iterations only (defaults to claude):

```bash
so run 5
```

Use docker explicitly:

```bash
SANDBOX=docker so run
```

Use bubblewrap:

```bash
SANDBOX=bwrap so run
```

## Commands

| Command | Description                |
| ------- | -------------------------- |
| `plan`  | Generate specs/prompt.md   |
| `run`   | Run agent in sandbox       |
| `step`  | Run with human-in-the-loop |
| `clean` | Fix code smells            |
| `dup`   | Remove duplicates          |
| `learn` | Guided learning session    |
| `menu`  | Manage existing sandboxes  |

## Workflow

1. `so plan` creates `specs/` directory with prompt template
2. Edit `specs/*.md` with any modifications
3. `so run` runs agent in isolated sandbox
4. Review changes with diff, shell into sandbox, reset and pick commit
5. Merge when satisfied and changes are squashed into your codebase

## Environment

| Variable  | Default  | Description     |
| --------- | -------- | --------------- |
| `SANDBOX` | `docker` | bwrap or docker |
| `MODEL`   | -        | Model override  |
| `EFFORT`  | -        | Effort override |

Set model and effort:

```bash
MODEL=openai/gpt-5.2-codex EFFORT=medium so run opencode
```
