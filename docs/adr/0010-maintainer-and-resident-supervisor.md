---
id: adr-0010
type: adr
title: 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する
status: accepted
created: 2026-09-22
updated: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - documentation
related:
  - design-overview
  - design-supervisor-lifecycle
  - design-persistence
  - design-plugin-integration
  - adr-0003
  - adr-0005
  - adr-0006
  - adr-0007
  - adr-0008
---

# ADR-0010: 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する

## Context

ドッグフーディング（ステップ9、journal 012〜014・019）を通して、登録・監視・レビュー・着地を行う常駐のClaude Code sessionを文書とpromptは「SV」「operator」「main session」と呼び分けてきた。runtimeの`supervise`プロセスも「supervisor」で、`SV`はその略に見える。さらにdesignとcodeの「operator」は、workspaceの`/exit`やstale登録の後始末のように人と常駐sessionのどちらが行ってもよい操作を指していて、After first dogfoodingで「SVの操作をruntimeへ移す」と書いた監視・応答の役割とも重なる。役割が3つ（runtimeのプロセス、常駐のClaude Code session、runごとのClaude session）あるのに名前が4つ以上あり、どれがどれを指すかを読み手が文脈から補っている。

起動手順も手作業に寄っている。cold startはAGENTS.mdの表に従って`cmux-taskq init`、`cmux workspace create --name TASKQ-SUPERVISOR --command "cmux-taskq supervise --parallel 4"`、常駐sessionのworkspace作成、pluginの読み込みを人が順に打つ。supervisorはcmux workspaceの中で動くので、cmuxを終了すれば止まり、killされれば`supervisors`表にstale登録が残り（`supervisors`表を足したとき、[ADR-0007](0007-run-level-leases-parallel-execution.md)のleaseと同じく「runtimeはstaleな行を自動で消さない」とした。[persistence](../design/persistence.md)のRuntime ownership）、再起動は人が`status`を見て判断する。supervisorのログはworkspaceの画面にしかなく、cmuxを閉じると消える。workspace名は`TASKQ-SUPERVISOR`と`taskq <task-id> <run-id>`で、複数のrepositoryで同時に流すと区別できない。CLIの使い方はAGENTS.mdの表とpluginのskillの両方にあり、常駐sessionの初期promptは人が最初に打つ文に依存する。

## Decision

1. **役割名。** runtimeの`cmux-taskq supervise`プロセスを**supervisor**、登録・監視・レビュー・着地を行う常駐のClaude Code sessionを**maintainer**、runごとにsupervisorが起動するClaude sessionを**worker**と呼ぶ。旧称の**SV**はmaintainerに読み替える。designとcodeでmaintainerを指していた**operator**もmaintainerと読む（人が同じ操作をしてもよい、という含みは変わらない）。CLIのsubcommand名（`supervise`）、ADR-0003の「supervisor」、`supervisors`表は変えない。
2. **cold startは`cmux-taskq up`の1コマンド。** supervisorはlaunchdのLaunchAgent（`KeepAlive`）として常駐し、cmux workspaceを持たない。`up`はcwdのrepositoryからqueueを解決し、queueごとのLaunchAgentのplistを書いて`launchctl bootstrap`し、続けてmaintainerのcmux workspaceを、runtimeが生成した初期promptを渡した`claude`で作る。maintainer workspaceは環境変数`CMUX_TASKQ_ROLE=maintainer`と`CMUX_TASKQ_QUEUE=<queue db path>`を持ち、`up`はこの環境変数の中（maintainer自身が`up`を打った場合）ではmaintainer workspaceを作らず、supervisorの起動だけを行う。
3. **停止は`cmux-taskq down`。** LaunchAgentを`launchctl bootout`してsupervisorにdrain（新しいclaimを止めてactive runの終了を待つ）させる。既定は即返り、`--wait`でdrainの完了まで待ち、`--force`で即殺する。maintainer workspaceは閉じない（maintainerは`down`の後も`show`やレビューを続けられる）。
4. **`up`はstale登録を消す。** `up`はsupervisor起動の前に、`supervisors`表のうちPIDが死んでいる登録行を削除する。これは`supervisors`表（schema v7）が[ADR-0007](0007-run-level-leases-parallel-execution.md)のleaseに倣って守ってきた「runtimeはstale登録を自動で消さない」の例外で、`up`だけに限る。`status`/`doctor`/`recover`/`integrate`は従来どおり消さず、`run_leases`（lease）には`up`も触らない。leaseの復旧は`recover`のままにする。
5. **cmux workspace名。** maintainerは`taskq <repo> maintainer`、workerは`taskq <repo> <task-id> <run-id>`。`<repo>`はrepository rootのbasename。複数repositoryを同じcmuxで流しても区別できる。
6. **supervisorのlog。** supervisorは起動ごとに`<queue dir>/logs/supervisor-<started_at>.log`へ書く（`<queue dir>`は[ADR-0006](0006-queue-per-repository.md)の`$XDG_DATA_HOME/cmux-taskq/<hash>/`）。`locate`の出力にlog dirを足す。ローテーションはせず、削除は人が行う。
7. **maintainerの初期promptとCLIの使い方の置き場。** maintainerの初期promptはruntimeが生成する（workerの`prompt.txt`と同じ位置付け。役割、queueの場所、最初に打つコマンドを含む）。CLIの使い方（登録・監視・レビュー・着地・復旧の手順）はpluginのskill `taskq-maintain`が持ち、AGENTS.mdはこのrepository固有の注意（バイナリの固定、テストの制約、文書のルール）だけを持つ。同じ手順をAGENTS.mdの表とskillの両方に書くことをやめる。

実装は3 taskに分ける: 役割名の統一とこのADR（本task）、`up` / `down`とlaunchd常駐（T2）、plugin skillのmaintainer化とAGENTS.mdの縮約（T3）。

## Alternatives

- **runtime側を改名する（`supervise` → `serve`など）**: 常駐sessionをsupervisorと呼べるようになるが、CLIのsubcommand、[ADR-0003](0003-supervisor-owns-lifecycle.md)以降の全ADRと設計文書、`supervisors`表、固定バイナリ（`~/.local/bin/cmux-taskq`）と動いている常駐プロセスに波及する。名前の衝突は文書側の改名で解ける。
- **workspace名だけを変える**: `TASKQ-SUPERVISOR`を`TASKQ-MAINTAINER`にするだけでは、文書とpromptのSV / operatorの混在が残り、起動手順の手作業とログの消失も解決しない。
- **常駐session側をoperatorと呼ぶ**: 既存の文書がoperatorをこの意味でも使っていて改名の量は少ないが、operatorは「監視して応答する役割」を指し、それはAfter first dogfoodingでruntimeへ移す予定の部分そのものになる。移した後もレビューと着地の判断を持ち続ける役割の名前としてmaintainerを選ぶ。
- **自前のwatchdogでsupervisorを再起動する**: cmux workspace内のsupervisorを別プロセスが監視して再起動する案。watchdog自体を守れず、cmuxの終了で一緒に止まる。launchdの`KeepAlive`はOSが守るので、守る対象を1つ減らせる。
- **supervisorもcmux workspaceに残す（launchdなし）**: 画面でログが見えるが、cmuxの終了で止まり、killされた後の再起動を人が判断する。ログはファイルに書けば`read-screen`は要らない。

## Consequences

- 文書はSVをmaintainer、maintainerを指すoperatorをmaintainerに置き換える。既存のADR（0001〜0009）とjournal（001〜021）は書き換えず、旧称は[overview](../design/overview.md)の用語集で読み替える。
- `up` / `down` / launchd / workspace名 / logは本ADRの時点で未実装で、T2が実装する。それまでのdesign文書は「T2で実装予定、ADR-0010」と書き、実装済みのように書かない。AGENTS.mdの手順表はT3まで残る。
- launchdはmacOS専用で、[ADR-0002](0002-cmux-first.md)のcmux前提と同じ範囲に留まる。他OSでの常駐は後続で扱う。
- `up`が`supervisors`表を掃除するので、「runtimeはstale登録を消さない」は「`up`以外は消さない」に狭まる。killされたsupervisorの登録は`up`まで`status`/`doctor`に残り、`up`が消したことはイベントかlogに残す（T2で決める）。
- supervisorのlogが起動ごとに増える。ローテーションはしないので、`logs/`の肥大は人が管理する。
- maintainerの初期promptとworkerのpromptがどちらもruntime生成になり、役割の記述が1か所（`src/runtime.rs`）に集まる。AGENTS.mdの手順表が消えると、CLIの使い方の正はskill `taskq-maintain`になり、CLIを変えるtaskはskillの更新を含める。
- 環境変数`CMUX_TASKQ_ROLE`と`CMUX_TASKQ_QUEUE`はmaintainer workspaceにだけある。`CMUX_TASKQ_DB`（launcherの明示override）とは別で、`up`の再入判定にだけ使う。
