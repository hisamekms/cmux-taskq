---
id: journal-021
type: journal
title: Role names (supervisor / maintainer / worker), `up` / `down`, and a launchd-resident supervisor
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: null
queue_task: null
depends_on_journal: [20]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
  - cargo llvm-cov --locked --fail-under-lines 80
related:
  - plan-rust-runtime-mvp
  - adr-0003
  - adr-0005
  - adr-0007
  - design-supervisor-lifecycle
  - design-persistence
  - design-plugin-integration
---

# 021: Role names (supervisor / maintainer / worker), `up` / `down`, and a launchd-resident supervisor

## Goal

[plans/current.md](../plans/current.md) の After first dogfooding 1項目目（詰まりの修正）と3項目目（SV の操作を runtime へ移す）の一部。次の3点を task に分けて `cmux-taskq add` に渡せる形で定義する。

1. 役割名を統一する。runtime の `supervise` プロセスは **supervisor**、常駐の Claude Code session（旧称 SV）は **maintainer**、run ごとの Claude session は **worker**。design と code で maintainer を指していた **operator** も maintainer に寄せる。
2. cold start を1コマンドにする。`cmux-taskq up` が supervisor を launchd の LaunchAgent として常駐させ（cmux workspace は持たない）、maintainer の cmux workspace を初期 prompt 付きの `claude` で作る。`cmux-taskq down` が supervisor を止める。
3. maintainer の手順を配布物で賄う。初期 prompt は runtime が生成し（worker の prompt と同じ位置付け）、CLI の使い方は plugin の skill が持ち、AGENTS.md はこの repository 固有の注意だけに縮める。

登録・実行・着地は maintainer が行う。この journal 自体は task に紐付けず（`queue_task: null`）、3 task の定義と経過の記録として使う。

## Decisions (2026-09-22, planning session with the user)

| # | 項目 | 決定 |
| --- | --- | --- |
| 用語 | 役割名 | supervisor（runtime）、maintainer（常駐 Claude session）、worker（run session） |
| 1 | `up` の入口 | `up` が自動判定する。maintainer workspace は `CMUX_TASKQ_ROLE=maintainer` と queue を示す `CMUX_TASKQ_QUEUE` を env に持って起動され、`up` はその env の中では maintainer workspace を作らない（plugin skill から呼んでも二重にならない） |
| 2 | stale な `supervisors` 行 | `up` が PID の死んだ登録行を消してから起動する。lease は触らない |
| 3 | `down` | 既定は即返り。`--wait` で drain 完了まで待ち、`--force` で即殺 |
| 4 | workspace 名 | `taskq <repo> maintainer`、`taskq <repo> <task-id> <run-id>`（複数 repository で同じ cmux を使うため） |
| 5 | 自動再起動 | launchd LaunchAgent。`up` が queue ごとの plist を書いて KeepAlive で起動、`down` は bootout する（KeepAlive のため signal だけでは再起動される） |
| 6 | log | 起動ごとに別ファイル `supervisor-<started_at>.log`。`locate` に log dir を出す。ローテーションはしない |
| 7 | task 13 との順序 | plugin task は 13（taskq skill が goal を登録する）に依存させる。同じ skill ファイルを触るため |
| 8 | ADR | ADR-0010 に1本でまとめる |
| 9 | operator 文言 | runtime task に同梱して maintainer に直す |
| 10 | 分割 | 3 task。(a) docs + ADR、(b) runtime、(c) plugin + AGENTS.md。(c) は (a)(b)(13) に依存 |

退けた案: runtime 側の改名（`supervise` → `serve` など。CLI・ADR-0003・固定バイナリに波及）、workspace 名だけの変更（文書の混在が残る）、session 側を operator にする（残っていく判断の役割より、runtime へ移す監視・応答の役割を指す）、自前 watchdog プロセス（watchdog 自体を守れない）。

`up` の前提: `up` を叩いた shell の PATH を plist の `EnvironmentVariables` に写すので、`~/.local/bin` と `claude` が見える shell から叩く。`up` はそれを preflight で確認する。

### T1 (a) = task 14: 役割名の統一と ADR-0010

```sh
cmux-taskq add "docs: unify role names as supervisor / maintainer / worker and add ADR-0010" \
  --description "..." --acceptance "..." \
  --verify "! grep -rnE '\\bSV\\b' docs/design docs/plans docs/README.md docs/journal/README.md" \
  --verify "test -n \"\$(ls docs/adr/0010-*.md)\""
```

### T2 (b) = task 15: runtime の `up` / `down`

```sh
cmux-taskq add "runtime: up and down start the maintainer workspace and a launchd-resident supervisor" \
  --description "..." --acceptance "..." \
  --verify "cargo fmt --all --check" --verify "cargo test --locked" \
  --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --verify "cargo llvm-cov --locked --fail-under-lines 80"
```

runtime を変えるので、maintainer は `integrate` の前に run の worktree で `cargo test --locked --test e2e -- --ignored` を通し、着地後に固定バイナリ `~/.local/bin/cmux-taskq` の更新をユーザーに報告してから入れ替える。

### T3 (c) = task 16: plugin の `taskq-maintain` skill と AGENTS.md の縮小

```sh
cmux-taskq add "plugin: taskq-maintain skill and repository-specific AGENTS.md" \
  --description "..." --acceptance "..." \
  --verify "cargo test --locked --test plugin" \
  --verify "claude plugin validate plugins/claude-taskq" \
  --verify "! grep -rnE '\\bSV\\b' AGENTS.md README.md plugins" \
  --depends-on 14 --depends-on 15 --depends-on 13
```

各 task の `--description` と `--acceptance` の全文は `cmux-taskq show <ID>` が正。

## Log

### 2026-09-22 claude (planning session)

- ユーザーとの壁打ちで上の Decisions を確定し、承認後に T1 = task 14、T2 = task 15、T3 = task 16（依存 13, 14, 15）を登録して ready にした。この journal の commit は TASKQ-SV session に依頼した。

## Result

閉じるときに書く。

## Promoted

- 
