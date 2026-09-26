# Paths and light verification

`integrate` runs a task's `--verify` commands once, serially, after its rebase, so a long check holds every landing behind it. Register a task with the checks its kind of change needs, and declare with `--paths` what it may change so the runtime catches a run that changed more (ADR-0029).

## Globs

`--paths GLOB` (repeatable) is matched against the whole path from the repository root. `*` and `?` stay inside one directory; a segment that is exactly `**` spans any depth. `*.md` is Markdown at the root only, `docs/**` everything under `docs/`, `**/*.md` Markdown anywhere. Blank, absolute (`/...`) and `.` / `..` / empty segments are refused. Without `--paths` a run may change anything.

## Recommended combinations

Follow the repository instructions first; for this repository:

| Change | `--paths` | `--verify` | `--evidence` |
| --- | --- | --- | --- |
| Docs only | `'docs/**'`, `'*.md'` | `'cargo fmt --all --check'` (or none) | none |
| Plugin docs and skills | `'plugins/**'`, `'docs/**'`, `'*.md'` | `'cargo test --locked --test plugin'` (`tests/plugin.rs` checks the skills) | none |
| Runtime (`src/`, `tests/`, `migrations/`) | none | fmt, clippy, `cargo llvm-cov --locked --fail-under-lines 80` (it runs every test, so no separate `cargo test`) | `e2e` |

A task that mixes kinds takes the verification of the heaviest kind.

The `--verify` commands are integrate's gate, not the worker's checklist: the worker prompt shows them as what integrate runs once after its rebase and tells the session to run the checks the repository's instructions ask of a worker (the `--verify` commands only when the instructions name none). In this repository a worker runs fmt, `cargo test --locked`, clippy and the task's `--verify` commands other than `cargo llvm-cov`; the coverage gate runs only in integrate (a run resumed because integrate's verification failed may rerun the failing command to reproduce it). A runtime run still runs e2e itself and writes it into the receipt's `e2e`.

## What happens outside the paths

- Validation compares the receipt's commit with where the branch forked from the current `main` (`git merge-base`; the base commit unless a resumed session rebased), so paths other tasks landed are never counted. A changed path no glob matches parks the run as `needs_session` with a `scope_violation` event (`paths`, `allowed`, `reason`); the supervisor resumes the session to restore those paths to their state at `git merge-base HEAD <main>`.
- `integrate` checks the diff it would squash onto `main` after its rebase the same way. Outside paths defer the run (`integration_deferred` with `scope_violation`) without moving `main` or running the verification.
- If the task truly needs another path, the session writes a `failed` receipt naming it. Register the task again with wider `--paths` and the verification that path needs.

## Change the paths

```sh
"$DAGQ" set-paths TASK --paths 'docs/**' --paths '*.md'   # replace every glob of a draft or ready task
"$DAGQ" set-paths TASK --none                             # remove the limit
```

Like `set-goal`, only a `draft` or `ready` task can change; a claimed run is checked against the paths it started with. A change records `task_paths_changed` (`from`, `to`).
