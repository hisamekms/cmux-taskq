---
id: design-manual-smoke
type: design
title: Manual smoke of the paths that include real Claude
status: current
created: 2026-09-25
updated: 2026-09-26
last_verified: 2026-09-26
scope: operations
related:
  - adr-0036
  - design-supervisor-lifecycle
  - design-provider-lifecycle
---

# Manual smoke of the paths that include real Claude

実 Claude Code を含む経路は自動 test にしない（AGENTS.md の「テストの制約」）。`tests/e2e.rs` のハッピーパスは stub を provider にするので、実 Claude の起動・ダイアログ・resume と、異常系の組み合わせはこの手順で人（または人に頼まれた session）が確かめる。runtime の振る舞いを大きく変えたとき、Claude Code か cmux の版を上げたときに流す。結果は task の receipt の `summary`（または `note`）に、版・シナリオごとの結果・見つけた問題を残す。

手順は 2 つある。

- [故障経路のスモーク](#故障経路のスモーク): 使い捨て repository で異常系 6 シナリオを起こし、二重起動と成果の喪失が無いことを確かめる。
- [独立 task 1 件の完走](#独立-task-1-件の完走): この repository の本番 queue で、登録から着地まで人が DB を直さずに通ることを確かめる。

## 故障経路のスモーク

### 隔離

本番 queue と固定バイナリ `~/.local/bin/dagq` を汚さないため、すべてを scratch directory に閉じる。

- **バイナリ**: 確かめたい commit で `cargo build --locked` したものを scratch にコピーして使う。`target/` のバイナリで本番 queue を開かない（開いただけでは migrate しなくなった（ADR-0045 決定 5）が、状態を変えるコマンドで未着地の遷移を本番に持ち込まない。緩める範囲は ADR-0045 決定 18）。
- **repository**: `git init` した使い捨て repository。`seed.txt`（検証コマンドが見る）、3 行の `shared.txt`（衝突用）、`CLAUDE.md`、`.claude/settings.json`（`permissions.defaultMode: auto`）を commit しておく。scratch に置いた bare repository を `origin` にする（supervisor の着地は push まで行い、`origin` が無いと `push_failed` の attention になる。手で `integrate` するときは `--no-push` でもよい）。
- **queue**: 全コマンドを `XDG_DATA_HOME=<scratch>/xdg` で、repository を cwd にして打つ（queue は `<scratch>/xdg/dagq/<hash>/queue.db` に解決される）。これを 1 行の wrapper script（例 `tq`）にしておく。
- **supervisor**: 専用の cmux workspace で `supervise --parallel 2 --claude <agent>` を起動し、`--log-dir` か `tee` で log を残す。`up` は使わない（inbox / planner の workspace と launchd agent を作るため）。`--once` は付けない。
- **folder trust**: 実 Claude を使う前に、使い捨て repository の root で一度 `claude` を起動して trust dialog を承認する。worktree で dialog が出るかは親 repository の root が信頼済みかで決まる（[provider-lifecycle](provider-lifecycle.md#trust-prompt)）。承認しないと、最初の承認より前に起動した run session がすべて dialog で止まる。

### stub と実 Claude の使い分け

- **stub**（`tests/e2e.rs` の `STUB` を拡張した shell script）で runtime の状態遷移を見るシナリオ（2〜5）を流す。token を使わず、時間を制御できる。拡張の例: title の `[break]` で `seed.txt` を消して commit し検証を壊す、`[hang]` で commit も receipt も書かずに待つ、`delay=N` で N 秒待ってから commit する。
- **実 Claude**（`--claude` に実体の path）で、Claude 自身の終了と resume が絡むシナリオ（1、6）を流す。cmux の terminal の PATH では session ごとの shim が先に解決されるので、`--claude ~/.local/bin/claude` のように実体を渡す（AGENTS.md の「起動と停止」と同じ理由）。
- stub は headless の review / triage（`claude -p`）にも使われる。`tests/e2e.rs` の stub は本文に `E2E-REVIEW-PASS` を含む task の review だけを pass にし、それ以外は失敗するので、stub のシナリオでは review が `review by hand`、triage が失敗の attention になるのが期待どおり。review と triage の verdict まで見たいシナリオは実 Claude で流す。検証は `integrate` の 1 回だけなので、review で止まった run は検証まで進まない。stub で着地まで流すシナリオ（2〜5）は task の description に `E2E-REVIEW-PASS` を入れて review を pass させるか、`review by hand` になった run に人が `integrate ID` を打つ。

### シナリオ

各シナリオで、`show ID`・`status`・`doctor` の出力、run のイベント列、保持されたリソース（`git worktree list`、run branch、run dir、cmux workspace）を記録する。runtime の振る舞いの正は [supervisor-lifecycle](supervisor-lifecycle.md) で、下の「確認点」はそれと食い違えば食い違いを問題として記録する。

| # | 起こすこと | agent | 確認点 |
| --- | --- | --- | --- |
| 1 | Claude の異常終了: commit した直後・receipt の前に、`doctor` の agent pid を `kill -KILL` | 実 Claude | wrapper が `session_exited`（signal 終了は exit 128）を記録し run は `failed`。worktree・branch・run dir が残る。復旧 job が retry / retry_inherit / resume / wait か escalate（`decide` の ask）を決める。並行する run と、空いた slot の次の claim に影響しない |
| 2 | supervisor の再起動: 2 run が `running` の間に supervisor を `kill -KILL` し、同じ workspace で新しい supervisor を起動 | stub | 新しい supervisor は wrapper が生きている run の stale lease を引き継ぎ（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）、run は receipt → 検証 → review まで進む。同じ task に 2 本目の run が立たない |
| 3 | 検証の失敗: `[break]` の task に `--verify 'test -f seed.txt'` | stub | 検証は `integrate` の 1 回だけで、失敗すると run は `needs_session` になり、supervisor が resume する（[`needs_session`](supervisor-lifecycle/needs-session.md#needs_session)）。3 回で解消しなければ `failed` と `decide` の ask。`main` は進まない。`tests/e2e.rs` の stub の resume は `set -eu` の下で `test -f seed.txt` を実行して非0で終わるので、拡張した stub の resume では `[break]` のときに `seed.txt` を戻す（解消を見る）か、壊したまま receipt を書き直す（回数上限を見る）かを決めておく |
| 4 | cleanup の失敗: review が pass して session が終わった（`session_exited`）後、supervisor が workspace を閉じる前に `cmux workspace close <uuid>` で閉じる | stub | supervisor の close は `not_found` で `cleanup_failed` イベントと `last_error` になり、run の状態は変わらず、着地も通る。supervisor は落ちない。worker の session は review の後まで開いたままなので（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）、それより前に閉じると wrapper が死んでシナリオ 5 と同じ abandon の経路になる。窓は短いので、コマンド終了後に cmux が workspace を自動で閉じる環境（[観測済みの環境依存](#観測済みの環境依存)）ではこれが自然に起きる |
| 5 | 並列中の 1 run の異常: `[hang]` と `delay=60` の 2 task を同時に流し、`[hang]` の wrapper を `kill -KILL` | stub | heartbeat 切れ（30 秒）で `[hang]` の run だけが手放され（`runtime_error`、lease 削除）、`recover run` の attention になる。孤児になった agent が生きている間は `recover` が拒否し、agent を止めると supervisor が `interrupted` にして triage に回す。もう一方の run は影響なく review（`E2E-REVIEW-PASS` が無ければ `review by hand`）まで進む |
| 6 | merge queue の衝突: 2 task が `shared.txt` の同じ行を変える | 実 Claude | 先に着地した run の後、もう一方は着地前の `merge-tree` の事前判定か `integrate` の rebase で衝突を検出し、生きている（または resume した）session に解消を依頼する。session が rebase・解消・receipt の書き直しをして着地し、`main` は 1 task 1 commit の直線になる。review が `concern` を返したら `approve_landing` の ask に答える |

全シナリオの後に確かめること:

- 二重起動が無い（同じ task に同時に 2 本の未完了 run が無い）。
- 成果が失われていない（commit した run の branch か `refs/dagq/runs/<run-id>` が残る）。
- `doctor` の `unfinished_runs` と `run_leases` が空になり、`pgrep` で Claude・stub・wrapper が残っていない。
- 作った cmux workspace（supervisor と、閉じられずに残った run のもの）を `cmux workspace close <uuid>` で閉じる。

### 観測済みの環境依存

- **workspace はコマンド終了後も残る**（cmux 0.64.25 (106)、2026-09-22）。`cmux workspace create --command` はコマンドをログインシェルに打ち込む形で起動し、wrapper が終わってもシェルと workspace は残る（`--command true` / `sleep 1` の probe で 5 秒以上残った）。同じ版の別の環境では 1〜2 秒後に workspace が自動で閉じたことがあり、cmux の設定に依存するとみられる。どちらでも supervisor の手順は同じ（[Cleanup and recovery](supervisor-lifecycle/cleanup-and-recovery.md#cleanup-and-recovery)）。
- **wrapper が SIGKILL で死ぬと** `run_processes.exited_at` は wrapper 行も agent 行も null のまま残る。`doctor` は PID の生死で補うので判定は変わらない。
- **未確認のまま残っているもの**: `exit_request_timed_out` の実機、検証コマンドの 30 分 timeout、`main` を進めた後の DB 更新失敗（[ADR-0008](../adr/0008-merge-queue-squash-landing.md) の既知の限界）。

## 独立 task 1 件の完走

この repository の本番 queue で、依存の無い小さな task（docs の誤りの修正など）を 1 件流し、登録から着地まで DB を手で直さずに通ることを確かめる。本番 queue の操作は固定バイナリ `~/.local/bin/dagq` だけで行う。

1. `which dagq` が `~/.local/bin/dagq` に解決し、`dagq --version` が確かめたい版であることを見る。固定バイナリの入れ替えが要るなら、AGENTS.md の「作業中」の手順で人に報告してから行う。
2. planner の session で task を 1 件登録する（`dagq add` に title・description・acceptance・`--verify`、docs だけなら `--paths`）。返った ID だけに `dagq ready` を打つ。
3. supervisor が居なければ inbox か planner の session から `dagq up --in-cmux ...`（AGENTS.md の「起動と停止」）。
4. 経過は `dagq show ID` と inbox の `watch` で見る。期待する流れ: claim → `[<repo>]worker#<task-id> - <title>` の workspace で worker が作業 → commit と receipt → validating → headless の review → pass なら `/exit`・workspace の close → `integrate`（rebase・検証・squash）→ push → task `completed`、run `integrated`。
5. 人の操作が要るのは、worker が止まったダイアログ（`answer_prompt` の ask）、review の `concern`（`approve_landing` の ask）、`review by hand`、`push_failed` だけで、どれも inbox に届く。それ以外で止まったら詰まりとして記録する。
6. 着地した commit（`Dagq-Task` / `Dagq-Run` trailer）、`refs/dagq/runs/<run-id>`、worktree と branch の削除、`origin/main` への push を確かめ、所要時間と詰まりを残す。

2026-09-22 の初回（固定バイナリ 0.1.0、README の手順の追記 1 件）は、登録から着地まで約 5 分、DB を手で直さずに通った（当時は review と `integrate` を常駐 session が手で行った）。worktree が信頼済みの repository のものだったので trust dialog は出なかった。
