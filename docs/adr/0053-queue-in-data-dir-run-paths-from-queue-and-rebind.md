---
id: adr-0053
type: adr
title: repositoryごとのqueueをデータディレクトリに置き、runのpathをqueueから解決し、repositoryとqueueの移動をrebindで扱う
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0006
  - adr-0017
  - adr-0020
owners:
  - hisamekms
tags:
  - persistence
  - cli
  - repository
  - runtime
  - operations
related:
  - adr-0006
  - adr-0017
  - adr-0020
  - adr-0032
  - adr-0042
  - adr-0045
  - adr-0052
  - design-persistence
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-rebind
  - design-plugin-integration
---

# ADR-0053: repositoryごとのqueueをデータディレクトリに置き、runのpathをqueueから解決し、repositoryとqueueの移動をrebindで扱う

## Context

queueの場所と、queueやrepositoryを動かしたときの扱いは3本のADRに分かれていた。

- [ADR-0006](0006-queue-per-repository.md)（2026-09-22）: CLIが`--db PATH`を必須とし、pluginのlauncherが`<Git common dir>/`の下にqueueを置いていたため、`.git`配下に実行時ファイルが増え、`supervise`と`integrate`は`--repo`で同じrepositoryを二重に指定していた。1つのrepositoryに1つのqueueがあれば足り、supervisor・worker・pluginのskillがどのworktreeからでも同じqueueに着くことが重要だったので、queueをユーザーのデータディレクトリに置いてcwdから解決し、`init`でrepositoryに束縛した。
- [ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md)（2026-09-23）: `task_runs`の`run_dir`・`worktree_path`・`receipt_path`・`log_path`はprovision時の絶対pathで保存され、全コマンドがそれをそのまま使っていたので、queueディレクトリを動かすと未完了のrunと着地待ちのrunがworktreeもreceiptも見つけられなくなった。linked worktreeのrepository側の`gitdir`も旧pathを指し、`git worktree remove`と`branch -D`が通らなくなる（`git worktree repair <新しいpath>`で直り、`git worktree prune`の後は戻せない。git 2.39で確認）。
- [ADR-0020](0020-rebind-queue-to-a-moved-repository.md)（2026-09-23）: queueは`queue_repository`でGit common directoryに束縛され、束縛は暗黙に付け替えられないので、repositoryを動かすと全コマンドが`queue is bound to another Git repository`で落ちた。改名の切り替えではこれをsqlite3の直接の`UPDATE`で乗り切っており、「DBは手で直さない」の唯一の例外になっていた。main working treeを動かすとrunのworktreeの`.git`ファイルも旧repositoryを指したままになる（新しいrepositoryで`git worktree repair <worktree>`を実行すると直る）。

ADR-0006のデータディレクトリ名（改名前の名前）と、launcherが見る環境変数の名前は改名（今は[ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md)の決定7）に上書きされ、「`init`で束縛し、以後の全コマンドがopen直後に検査する」はADR-0020の決定1で`rebind`だけが例外になった。ADR-0017とADR-0020の決定はすべて有効のまま、同じ主題のADR-0006と読み合わせないと全体が分からない。

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の規則に従い、このADRはADR 0006・0017・0020を丸ごと置き換える（[ADRの棚卸し](../plans/adr-inventory.md)の組B）。3本の生きている決定を今の名前で書き直して引き継ぎ、上書きされた決定は今の形で書く。書き直しでは今の実装（[persistence](../design/persistence.md)、[`rebind`](../design/supervisor-lifecycle/rebind.md)、`src/infrastructure/location.rs`・`src/application/rebind.rs`・`src/main.rs`）に合わせた。新しい決定は足さない。

## Decision

### queueの場所と解決

1. **queueはrepositoryごとに1つ、ユーザーのデータディレクトリに置く。** 場所は`$XDG_DATA_HOME/dagq/<hash>/queue.db`（`XDG_DATA_HOME`が未設定・空・相対pathなら`$HOME/.local/share`。場所は`QueueLocation`が決める）。`<hash>`はcanonicalizeしたGit common directoryのUTF-8 bytesのSHA-256のhex先頭16文字で、symlink経由でもworktreeからでも同じになる。同じディレクトリに`repository`ファイル（束縛先のcommon directoryのpath）を置き、人がhashから逆引きできるようにする。データディレクトリ名`dagq`はADR-0052の決定7の対応表の値。
2. **run dirとlogはqueueと同じディレクトリに置く。** runのworktree・receipt・log・画面は`<queue dir>/runs/<run-id>/`、processのlogは`<queue dir>/logs/`に置く。`--db PATH`で開いたqueueでも`dirname PATH`/`runs/`と`dirname PATH`/`logs/`にし、規則を1つにする。`logs/`のファイルの書式は[ADR-0033](0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)と[persistence](../design/persistence.md)が持つ。
3. **cwdから解決し、`--db`と`--repo`はoverrideにする。** CLIは`--db`がなければcwdの`git rev-parse --path-format=absolute --git-common-dir`からqueueを解決するので、どのworktree（runのworktreeを含む）からでも同じqueueに着く。`--db PATH`は使い捨てのrepositoryとtestのための明示のoverrideとして残す。`supervise --repo`と`integrate --repo`は任意のoverrideで、既定はcwdのcheckout。
4. **queueをrepositoryに束縛し、開くたびに検査する。** cwdから解決したqueueは`init`がDBのディレクトリと`repository`ファイルを作り、`queue_repository`にcommon directoryを束縛する（`bind_repository`）。以後、queueを開くコマンドはopen直後に一致を検査し（`assert_repository`。読み取り専用のopenと`doctor`も同じ）、一致しなければ`queue is bound to another Git repository`で失敗する。`--db`のqueueは最初の`supervise`が束縛し、`supervise`と`integrate`が検査する。`bind_repository`は別のrepositoryへの束縛を拒否し、暗黙の付け替えはどこでも起きない。例外は束縛を付け替える`rebind`（決定10）だけで、`rebind`はopen直後の検査を通らない。DBを開かない`locate`（決定5）と、schemaの移行とバイナリの入れ替えを扱う`migrate`・`install`（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)）は束縛を検査しない。通常の操作は存在しないDBを暗黙に作らない。
5. **`locate`が解決結果をDBを開かずに返し、launcherはpathを計算しない。** `locate`は`db`、`queue_dir`、`runs_dir`、`log_dir`、`label`、`launch_agent`、`source`（cwdから解決したか`--db`か）、`git_common_dir`（`--db`ならnull）、`db_exists`を返す。pluginのlauncher（`plugins/claude-dagq/bin/dagq`）はDBのpathを計算せず、cwdのままバイナリを呼び、`DAGQ_DB`があるときだけそれを`--db`として渡す。skillは`--db`と`--repo`を渡さず、cmux workspaceのcwdをrepositoryにするだけで同じqueueに着く。

### runのpath

6. **queue配下のpathはDBの値を信用せず、run IDとqueueディレクトリから解決する。** `SqliteQueue`は開いたDBのpath（正規化した絶対path）から`runs_dir`を決め、`task_runs`の行を読むたびに`run_dir`・`worktree_path`・`receipt_path`・`log_path`を`<runs_dir>/<run-id>/…`に置き換える（`TaskRun::relocated`、配置は`RunPaths`の1か所）。値がnull（まだprovisionしていないrun）ならnullのまま。`repo_path`はqueueではなくrepositoryを指すので対象外（repositoryの移動は決定10〜16）。idle markerとClaudeのsettingsは`run_dir`から導出し、列を持たない。
7. **列とその書き込みは残し、schemaは変えない。** provisionは今までどおりその時点の絶対pathを列に書き、列の値は「claim時にどこへ作ったか」の記録になり、読み出しでは使わない。runの更新は置き換え前のpathを保存するので、読み出しの置き換えが列を書き換えることはない。相対pathへ書き換えるmigrationを採らないのは、schema versionを上げると、同じqueueを開いている古いバイナリが動けなくなるから（互換の範囲の扱いはADR-0045）。既存のqueueに移行作業は無い。
8. **worktreeのGit管理情報は`integrate`が直す。** 着地（`land`）はworktreeの存在を確かめた直後に、repositoryの主working treeで`git worktree repair <worktree>`を実行する。移動していなければ何もしないので毎回実行する。これで着地後の`worktree remove`と`branch -D`が新しいpathで通る。supervisorの検証（`git -C <worktree>`の読み取り）は`.git`ファイル経由で動くのでrepairしない。
9. **queueディレクトリの移動の手順。** supervisorを止め（`down --wait`で走行中のrunを終わらせる）、queueディレクトリの中身（`queue.db`とWALファイル、`runs/`、`logs/`、`repository`）をまとめて移し、移した先で`up`し直す。走行中のrunのcmux workspaceは旧pathの`runner`と`--db`で動いているので、runを走らせたまま動かすことはサポートしない。`needs_session`のrunは移動前に解消して着地させる（再開するClaude sessionは旧worktreeのcwdと、promptに書かれた旧receiptのpathを覚えているので、移動後の`--resume`が同じsessionを見つけ、新しい場所にreceiptを書く保証が無い）。移動から`integrate`までの間に`git worktree prune`を打たない（管理情報が消えて`repair`で戻らない）。動かしたworktreeを手で片付けるときは先に`git worktree repair <path>`を打つ。

### repositoryの移動と`rebind`

10. **束縛を変える明示のサブコマンド`rebind [--repo PATH]`を持つ。** 開いたqueueの`queue_repository`を、cwd（または`--repo`）のrepositoryのcanonicalなGit common directoryに付け替える（`rebind_repository`が1トランザクションで旧値を読み、新しい値を書く）。`rebind`だけはopen直後の検査（決定4）を通らない。`bind_repository`は変えず、`init`・`supervise`・その他のコマンドは今までどおり別のrepositoryへの束縛を拒否する。束縛の無い`--db`のqueueは`rebind`で束縛される。
11. **出力。** `outcome`（`rebound`、同じrepositoryなら`unchanged`）、`db`、`previous_git_common_dir`と`git_common_dir`（旧と新）、`queue_dir`、`repository_queue_dir`（新しいcommon directoryから解決されるqueueディレクトリ。data homeが決まらなければnull）、`move_to`（`repository_queue_dir`が`queue_dir`と違うときだけその値、同じかdata homeが決まらなければnull。行き先が既にあるかは見ない）、`worktrees`（決定13）。
12. **記録。** 変わったときだけ、`<queue dir>/logs/rebind.jsonl`に`{"at", "previous_git_common_dir", "git_common_dir", "binary_version"}`を1行追記し、queueディレクトリに`repository`ファイルがあれば新しいpathに書き換える。`run_events`には書かないので、`events`・`watch`には出ない（採用した時点の`run_events`はtaskかgoalの無いeventを受け付けず、入れるにはschemaを変えるmigrationが要った。記録の置き場所の分類は[ADR-0032](0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)が扱う）。
13. **worktreeのGit管理情報を直す。** 付け替えの後、DBのrunのうちworktree（queueの今の`runs/`から解決したpath、決定6）がディスクに残っているものすべてに、新しいrepositoryで`git worktree repair <worktree>`を実行する。失敗しても`rebind`は失敗せず、`worktrees`の各項目（`run_id`、`worktree_path`、`repaired`、`error`）に`repaired: false`と`error`を出す。これで`needs_session`のrunを再開するsessionもworktreeでGitを使える。この後でqueueディレクトリを動かしたときは、`integrate`の`land`がもう一度repairする（決定8）。
14. **走行中のsupervisorか着地中の`integrate`がいれば拒否する。** 登録された`supervisors`のうちPIDが生きているもの（heartbeatが古いhung状態を含む）が1つでもあれば、`down --wait`で止めるよう求めて失敗する。PIDの死んだ登録（killされた残り）は無視する。`integrating`のrunのleaseのPIDが生きていれば（着地中の`integrate`）同じく失敗する。どちらも旧repositoryのpathを持って動いているため。拒否したときは束縛を変えない。
15. **repositoryとqueueを動かす手順。** READMEの「Move the repository or the queue」に書く。推奨は次の順。
    1. 旧checkoutで`down --wait`（走行中のrunを終わらせ、旧hashのLaunchAgentを外す）。`needs_session`のrunは先に解消して着地させる（決定9）。
    2. repositoryを動かす。新しいcheckoutで`init`を打たない（新しいhashの場所に空のqueueができ、移すqueueとぶつかる）。
    3. 新しいcheckoutで旧queueを`--db`（pluginのlauncherなら`DAGQ_DB`）で指して`rebind`する。拒否されるならここで止まり、何も動いていない。
    4. `rebind`の`move_to`へqueueディレクトリを丸ごと動かす（決定9の移動と同じ）。
    5. 新しいcheckoutで`list`・`status`を確かめ、`up`で戻す。

    逆の順（queueディレクトリを新しいhashの場所（`locate`の`queue_dir`）へ先に動かし、フラグ無しで`rebind`）でも動く。その間は`init`を含む全コマンドが束縛の不一致で拒否する。推奨を`rebind`先にするのは、拒否されうる操作を何も動かす前に済ませられ、動かす先を`move_to`がそのまま教えるから。
16. **DBを手で直す例外は無い。** repositoryの移動は`rebind`、queueディレクトリの移動は決定6〜9で扱い、sqlite3などでDBを直接操作しない。

### 旧ADRの決定からの対応

ADR-0006は決定に番号が無いので、Decisionの箇条を上から数えた番号（[ADRの棚卸し](../plans/adr-inventory.md)の「箇条N」）で示す。後のADRで、この3本の決定を番号で参照するのは[ADR-0032](0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)（`proposed`）の「ADR-0020の決定3」だけ（0036以降に番号での参照は無い。2026-09-26にgrepで確認）。

| 旧ADRの決定 | このADR |
| --- | --- |
| ADR-0006 箇条1（データディレクトリ、hash、`repository`ファイル） | 決定1（ディレクトリ名は`dagq`） |
| ADR-0006 箇条2（`runs/<run-id>/`、`--db`でも`dirname PATH`） | 決定2 |
| ADR-0006 箇条3（cwdから解決、`--db` / `--repo`はoverride） | 決定3 |
| ADR-0006 箇条4（`init`で束縛し、全コマンドが検査する） | 決定4（`rebind`だけが例外） |
| ADR-0006 箇条5（`locate`、launcherは環境変数があるときだけ`--db`） | 決定5（環境変数は`DAGQ_DB`） |
| ADR-0017 決定1（run IDとqueueディレクトリから解決する） | 決定6 |
| ADR-0017 決定2（列と書き込みを残し、schemaを変えない） | 決定7 |
| ADR-0017 決定3（`integrate`が`git worktree repair`する） | 決定8 |
| ADR-0017 決定4（移動の手順） | 決定9 |
| ADR-0020 決定1（`rebind`サブコマンド） | 決定10 |
| ADR-0020 決定2（出力） | 決定11 |
| ADR-0020 決定3（記録） | 決定12 |
| ADR-0020 決定4（worktreeのrepair） | 決定13 |
| ADR-0020 決定5（走行中の拒否） | 決定14 |
| ADR-0020 決定6（手順） | 決定15 |
| ADR-0020 決定7（DBの直接操作の例外が無くなる） | 決定16 |

## Alternatives

- queueを`<Git common dir>/`の下に置き続ける: `.git`配下にworktreeとlogが積まれ、`git worktree`の管理領域と混ざる。repositoryを消すとqueueも消え、複数のrepositoryのqueueを一覧できない。
- repositoryのpathをそのままディレクトリ名にする: 長く、区切り文字のescapeが要る。hashと`repository`ファイルの組で同じ情報を保てる。
- 1つのDBに全repositoryのqueueを入れる: queue単位のlease・束縛・schema migrationが複雑になる。repositoryをまたぐ依存は当面scope外。
- `--db`を廃止する: testと使い捨てのrepositoryでユーザーのデータディレクトリを汚さずに動かす手段が要る。`XDG_DATA_HOME`の差し替えでも可能だが、明示のoverrideの方が単純。
- runのpathの列を相対pathに書き換える、または列を消すmigration: schema versionが上がり、同じqueueを開いている古いバイナリを止める。新旧の行で意味が変わる列を持つことにもなる。記録として残す害は無い。
- 移動を検出するコマンドでDBのpathを書き換える: 移動のたびに人が打つ手順が増え、打ち忘れると同じ壊れ方をする。読み出しで解決すれば手順が要らない。
- worktreeのrepairを手順だけに書く: `integrate`の後始末が壊れ、手作業が残る。repairは冪等で安い。
- `init`に`--rebind`を足す: `init`は新しい場所に空のDBを作りうる。付け替えを別コマンドにしておくほうが、誤って空のqueueを作る経路と混ざらない。
- 束縛の不一致を見つけたら自動で付け替える: hash衝突やコピーしたデータディレクトリを黙って受け入れてしまう（束縛を置いた理由そのもの）。
- `rebind`がqueueディレクトリも動かす: WALファイルを含むディレクトリの移動を、runtimeが開いたDBの下で行うことになり、失敗すると半端な状態が残る。移動は人が丸ごと行い、`rebind`は行き先を示すだけにした。
- supervisorが走っていても付け替える: 走っているsupervisorは旧repositoryのpathで`main`を読み、worktreeを作り続けるので、束縛だけ変えても次の検査で壊れる。
- ADR 0006・0017・0020を置き換えずに残す: 改名前のデータディレクトリ名と環境変数名を書いたADR-0006が`accepted`のまま残り、単独で開いた読み手が今も有効と読む（ADR-0042）。

## Consequences

- 実行時ファイルはworktreeの外にあり、`.git`の管理領域と混ざらない。同じrepositoryを別のpath（symlinkを含む）から使ってもcanonicalizeで同じqueueになり、hash衝突やデータディレクトリの複製で別のrepositoryのqueueに当たっても束縛の検査が拒否する。
- repositoryを動かすとhashが変わり、新しいcheckoutは新しい空の場所に解決される。旧queueは`repository`ファイルで特定し、決定15の手順で`rebind`して移す。自動の移行は行わない。`rebind`を打つまでは、移動後のcheckoutから旧queueを使うコマンドは束縛の不一致で落ちる（`--db`で開いたqueueは`supervise`と`integrate`だけが落ちる）。
- queueディレクトリを動かしても、`show`・`status`・`doctor`・`recover`・`integrate`・検証・promptの前任者summaryが新しい場所のpathを返し、使う。DBの`run_dir`などの列は移動後は実際の場所と一致しないので、調べるときは`show`を使い、DBを直接読まない。`run_events`のpayloadに写したpath（`worktree_created`、`verification_command`の`log_path`など）は記録時点のまま書き換えない。
- `runs/<run-id>/`の配置を変えるときは、`RunPaths`と既存のrunの配置を同時に扱えるかを考える必要がある（今は配置が1通りなので、run IDだけから導ける）。
- `git worktree prune`を移動後・着地前に打つと、そのrunのworktreeは`integrate`できなくなる（`repair`が失敗し、`integrate`はmainを動かす前に止まってrunを元のstatusに戻す）。
- DBのpathを正規化してから`runs/`を決めるので、相対pathやsymlinkの`--db`で開いても、`supervise`が作った場所と同じ絶対pathが返る。
- `rebind`の履歴は`logs/rebind.jsonl`にだけ残り、`events`・`watch`には出ない。`task_runs.repo_path`は書き換えないので、既に作ったrunのcmux workspaceのtitleは変わらない。
- 旧hashのLaunchAgentは旧checkoutで`down`を打たないと残る。`rebind`はそれを外さないので、決定15の手順1を飛ばしたときは`launchctl bootout gui/<uid>/com.dagq.<旧hash>`とplistの削除が要る。
- ADR 0006・0017・0020は`superseded`になり、`superseded_by: adr-0053`を持つ。
