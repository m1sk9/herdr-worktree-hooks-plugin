# herdr-worktree-hooks-plugin

[![CI](https://github.com/m1sk9/herdr-worktree-hooks-plugin/actions/workflows/ci.yaml/badge.svg)](https://github.com/m1sk9/herdr-worktree-hooks-plugin/actions/workflows/ci.yaml)
[![Release herdr-worktree-hooks-plugin](https://github.com/m1sk9/herdr-worktree-hooks-plugin/actions/workflows/release.yaml/badge.svg)](https://github.com/m1sk9/herdr-worktree-hooks-plugin/actions/workflows/release.yaml)
[![Apache License 2.0](https://img.shields.io/github/license/m1sk9/herdr-worktree-hooks-plugin?color=%239944ee)](https://github.com/m1sk9/herdr-worktree-hooks-plugin/blob/main/LICENSE)
[![codecov](https://codecov.io/github/m1sk9/herdr-worktree-hooks-plugin/graph/badge.svg?token=QA075J11S8)](https://codecov.io/github/m1sk9/herdr-worktree-hooks-plugin)

A [herdr](https://herdr.dev) plugin that seeds a newly created git worktree from its main checkout: it copies gitignored files such as `.env` and then runs any setup commands you configure.

```shell
herdr plugin install m1sk9/herdr-worktree-hooks-plugin
```

[_Requires herdr v0.8.0_](https://github.com/herdrdev/herdr/releases/tag/v0.8.0)

## Features

- **Copies, never links.** The worktree gets its own file.
- **Never overwrites.** A file that already exists in the worktree is left alone.
- **Runs once per worktree.** Overlapping events cannot cause a double apply.

## Install

```
herdr plugin install m1sk9/herdr-worktree-hooks-plugin
```

For local development, `herdr plugin link` does *not* run `[[build]]`, so build first:

```
cargo build --release
herdr plugin link .
herdr plugin list
herdr plugin action invoke m1sk9.worktree-hooks.seed
```

The `seed` action is needed after linking because the `[[startup]]` hook only runs at session start — without it, worktrees that already exist stay unclaimed until herdr restarts.

## Configure

```
herdr plugin config-dir m1sk9.worktree-hooks
```

Drop a `config.toml` in that directory. See [`config.example.toml`](config.example.toml) for the full set of keys.

```toml
shell = "/bin/zsh"

[defaults]
copy = [".env", ".env.local"]

[repos.my-app]
copy = [".env.test"]
run = ["pnpm install --frozen-lockfile"]
```

With no config file at all, the plugin copies `.env` and `.env.local`.

### `copy`

Paths relative to the main checkout. Globs are not supported. A path that is absolute or contains `..` is rejected. Parent directories are created as needed, so `config/secrets.yml` works.

### `run`

Shell command strings executed in the new worktree, in order, with:

| Variable | Value |
| --- | --- |
| `MAIN` | main checkout (herdr's `repo_root`) |
| `WORKTREE` | new worktree (herdr's `checkout_path`) |
| `REPO` | repository name |
| `BRANCH` | branch name, empty if detached |
| `EVENT` | the herdr event that triggered the run |

A non-zero exit fails the run and releases the claim, so the next event retries.

## How it decides to run

The plugin subscribes to `worktree.created` and `workspace.created`. On herdr 0.8.0 the sidebar's New Worktree emits both within the same millisecond, and which one arrives first is a race, so both are handled and the paths are taken from whichever payload the winner carries. A payload that names only a workspace
id is resolved through `herdr workspace get`.

Two guards keep repeat firings harmless:

1. A `[[startup]]` hook claims every worktree that already exists when the session starts.
2. Outside of `worktree.created`, the checkout must have been created within `fresh_window_secs` (default 300) — otherwise opening an *existing* worktree, which also emits `workspace.created`, would look like a new one.

Each worktree is then claimed with an `O_EXCL` file under `$HERDR_PLUGIN_STATE_DIR/claims/` (on macOS, `~/.local/state/herdr/plugins/m1sk9.worktree-hooks/claims/`). The first event wins; the rest log a skip.

Only `worktree.created` reports the branch, so `$BRANCH` is read back with `git rev-parse` when the winning event did not carry it.

The main checkout itself (`is_linked_worktree = false`) is always skipped.

## Re-running by hand

The workspace action **Apply worktree hooks** re-applies to the focused workspace, claim or no claim:

```
herdr plugin action invoke m1sk9.worktree-hooks.apply
```

Existing files are still never overwritten.

## Troubleshooting

Every invocation logs the event name and the paths it resolved before deciding anything, which makes the log a usable event probe:

```
herdr plugin log list --plugin m1sk9.worktree-hooks
```


## LICENSE

herdr-worktree-hooks-plugin is published under [Apache License 2.0](./LICENSE).

<sub>
    ® 2026 m1sk9
</sub>
