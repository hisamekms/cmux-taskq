# Updating the fixed binary: install, auto-update and the update asks

Read this when the person wants to update or roll back the fixed `dagq` binary, turn on automatic updates, or answer an `update_failed` / `approve_update` ask (ADR-0073). Everything here is typed from the inbox or a planner session, on the person's word; tell the person before a binary is replaced.

A binary names itself by its build identifier (`dagq --version`): `X.Y.Z` for a release, `X.Y.Z-dev+<commit>[.dirty]` for a build of main. A supervisor is replaced whenever that identifier differs, so a rebuild from another commit is replaced like a release; no version bump is needed.

## install

```sh
"$DAGQ" install                    # build the queue's main checkout and put it in place
"$DAGQ" install --from PATH        # a checkout (built) or a binary file
"$DAGQ" install --rollback         # go back to <binary>.previous
```

`install` checks the new binary (`--version`, and `init` + `list` on a throwaway queue), applies the compatible migrations, puts it in place with a rename (the one it replaced stays as `<binary>.previous`), and hands every live supervisor over to it: each supervisor execs the new binary under its own pid and token at its next short step, so the sessions and runs in flight go on and nothing waits for them (`"outcome": "installed"` with `version`, `previous_version`, `migrated`, `supervisors`). If the handoff fails, `install` puts the previous binary back and says so. `--to PATH` replaces another file than the running `dagq`. Never replace the file with `cp`: on macOS that can kill the processes running it, and it leaves no `.previous` to go back to.

- A supervisor listed under `not_handed_off` (a binary from before the handoff, or a launchd agent whose plist names another path) is replaced by the next `up`, which drains it.
- `install` stops, replacing nothing, when a supervisor is live and the new binary itself predates the handoff (a `--rollback` to such a binary too): then `down --wait`, the same `install` (with no supervisor it replaces the file and applies the compatible migrations), and `up`.
- **A breaking migration** (the error names it; `"$DAGQ" migrate --check` lists each with `compatible`): `install` refuses unless `--allow-breaking`, which drains: it stops the supervisor with `down --wait` (it waits for its runs, and for runs waiting on an ask, so show the person the open asks first), copies the queue to `backups/`, migrates, replaces the binary and starts the supervisor again with `up` in the mode it had. Pass what that `up` needs: `--claude EXE`, `--cmux EXE`, `--plugin-dir "$CLAUDE_PLUGIN_ROOT"`. That `up` does not carry `--auto-update`: run your usual `up ... --auto-update` afterwards (it reuses the new supervisor and turns the setting back on). A `--rollback` past a breaking migration is refused; the error names the backup.
- `unsupported queue schema version ... install a newer dagq`: the queue was migrated past this binary; use the newer fixed binary. `run dagq migrate`: the binary has migrations the queue lacks; `install` or `up` applies the compatible ones, a breaking one goes as above.

## Automatic updates (`up --auto-update`)

```sh
"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT" --auto-update   # with the same other flags as always
```

`--auto-update` is written on the registration of the supervisor `up` starts or reuses, so turning it on needs no restart: an `up` whose build matches the running one answers `reused` and sets it. An `up` without the flag turns it off again, so every later `up` (including the `restart supervisor` one) must carry it. `status` shows `auto_update` (`enabled`, `state`, the last step) and each `supervisors[].auto_update`.

While it is on, each landing on main that changes `src/`, `migrations/`, `Cargo.toml`, `Cargo.lock` or `build.rs` is built under the queue's directory (not in the person's checkout), checked, installed like `install`, and the supervisor hands itself over; a watch puts the previous binary back if no handed-over supervisor heartbeats on the new one within 60 seconds (when only some fail, the new binary stays and only the failed ones are started again). Each step is a queue event (`"$DAGQ" events --kind update_installed`, `update_failed`, ...); `update_installed` reaches the inbox as `report the update`.

## The update asks

Both have no task and no run, and are asked by `supervisor`. A newer ask of the same kind closes the older one (`superseded`).

- **`update_failed`** (`retry` / `skip`): the question says at which stage the update failed (`build`, `check`, `install`, `watch`, or `interrupted` when the job died), the error, what became of the binary (put back, or `kept` when only some supervisors failed and the others run the new one) and of each supervisor, and the job's log. Show it as written. Answer with the person's option: `retry` builds main's head again at the next check (after the cause is fixed), `skip` waits for the next runtime landing. A live `--auto-update` supervisor applies the answer and closes the ask. With none, the answer comes back as `read the answer of ask <id> and close it`: nothing applies it; update by hand with `install` if the person wants, then `ask close <id>`. If no supervisor serves the queue after a failed update (`restart supervisor`), `up` starts one with the binary now in place.
- **`approve_update`** (`install` / `skip`): main built as a binary with a breaking migration, so nothing was installed; the build is kept under `<queue dir>/update/staged/dagq` and the question names the migrations and the exact command. The runtime never applies it. After writing the person's answer it comes back as `read the answer of ask <id> and close it`: on `install`, and on the person's word, run the command from the question as written (`"$DAGQ" --db <db> install --from <queue dir>/update/staged/dagq --to <binary> --allow-breaking ...`), which drains as described under install, then the usual `up ... --auto-update`, then `ask close <id>`. On `skip`, `ask close <id>`; the next runtime landing builds again.
