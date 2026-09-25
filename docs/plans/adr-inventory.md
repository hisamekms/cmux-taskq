---
id: plan-adr-inventory
type: plan
title: ADR 0001〜0034の棚卸しと統合ADRの組
status: active
created: 2026-09-25
updated: 2026-09-26
owners:
  - hisamekms
tags:
  - architecture
  - documentation
depends_on:
  - adr-0042
related:
  - adr-index
  - design-overview
---

# ADR 0001〜0034の棚卸しと統合ADRの組

[ADR-0042](../adr/0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の決定8（ADR-0035の決定8を引き継ぐ）の棚卸し。0001〜0034の各ADRの決定を1つずつ、後のADR・[overview](../design/overview.md)の用語集・現在の`docs/design/`と照らし、現在有効か、どのADRのどの決定に上書きされたかを調べた。結果から、上書きされた決定を1つでも含むADRを丸ごと置き換える統合ADRの組を決める。統合ADRの執筆は組ごとのtaskで行い、この計画ではどのADRも置き換えない（2026-09-25時点）。

## 読み方

- **決定**: ADRのDecisionの番号。番号の無いADRはDecisionの箇条（または段落）を上から数えた番号（「箇条N」）で示す。Consequences・切替手順は、決定として読まれうるものだけ取り上げる。
- **有効**: 今もそのとおり。表現の中の旧称（SV、maintainerなど）を用語集で読み替えれば正しいものも、内容が生きていれば有効とし、根拠にそう書く。
- **上書き**: 後のADRの決定が内容を変えた、または無くした。後継の欄に上書きしたADRと決定を書く。上書きしたADRがさらに置き換えられていれば、今の後継（例: ADR-0023 → ADR-0040）を併記する。
- **失効**: 一時的な制約や一度きりの手順で、今は効かない（後のADRが変えたものも含む）。
- **記録**: 実装の割り当て・schemaの番号など、その時点の事実の記録で、決定ではない。統合ADRは引き継がない。
- **未実装**: 後継の決定は`accepted`だが実装がまだのもの。ADRとしては後継の決定が現在の決定なので、上書きとして扱う。
- 対象外: 0032〜0034は`proposed`のまま（acceptedになるときにADR-0042の規則に従う）。0012・0014・0023・0024は置き換え済み（0012 → [ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)、0014 → [ADR-0045](../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)、0023 → [ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)、0024 → [ADR-0041](../adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)）で、表には後継だけを書く。ADR-0041は2026-09-26に[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)に丸ごと置き換えられ、決定1〜17は同じ番号で引き継がれた（決定4だけ内容が変わった）ので、この文書の「ADR-0041 決定N」はADR-0044の決定Nと読む。ADR-0044とADR-0019・ADR-0043は2026-09-26に[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)に丸ごと置き換えられた。ADR-0044の決定NはADR-0047の決定N、ADR-0019の決定NはADR-0047の決定23+N、ADR-0043の決定NはADR-0047の決定29+Nと読む（ADR-0047のContextの対応表）。ADR-0019が入っていた組D（0008、0019、0027）の統合ADRは、0019の代わりにADR-0047の決定24〜29を参照する。

## ADRごとの表

### ADR-0001 Rustでruntimeを実装する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 runtimeとsupervisorをRustで実装する | 有効 | — | crateはRust |
| 箇条1 単一の`cmux-taskq`バイナリとして配布する | 上書き | ADR-0015（対応表のcrate / バイナリ行） | バイナリ名は`dagq` |
| 箇条2 domain / application / infrastructureをcrateまたはmoduleで分ける | 有効 | — | ADR-0013が1 crateのmodule境界に具体化した |

### ADR-0002 cmuxを最初のworkspace backendにする

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 cmuxを必須のworkspace backendにする | 有効 | — | |
| 箇条1 プロダクト名を`cmux-taskq`とする | 上書き | ADR-0015 | 名前は`dagq` |
| 箇条2 domain / applicationはcmuxのAPIを直接参照せずportを介す | 有効 | — | `WorkspaceBackend`（overview） |

### ADR-0003 supervisorがagentとworkspaceのライフサイクルを所有する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 キューごとに一つのsupervisorを起動する | 上書き | ADR-0007 箇条2 | 同じqueueに複数のsupervisorが居てもよく、claimが直列化される |
| 箇条1 supervisorがworkspace作成・監視・完了検証・workspace削除を行う | 有効 | — | 閉じる時点はADR-0027 決定1でreviewの後になったが、所有者は変わらない |
| 箇条2 agentはworkspaceを削除せず、結果をreceiptで通知する | 有効 | — | AGENTS.mdのworker |

### ADR-0004 ClaudeとCodexをagent providerとして抽象化する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 applicationはsessionの契約だけを使い、CLI引数などはadapterに閉じ込める | 有効 | — | `AgentProvider`（overview） |
| 箇条2 TaskRunにrequested / actual providerを記録する | 有効 | — | [domain-model](../design/domain-model.md)、[provider-lifecycle](../design/provider-lifecycle.md) |

全決定が有効。統合の対象にしない。

### ADR-0005 runtimeをバイナリ、agent integrationをpluginとして配布する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 runtimeを`cmux-taskq`バイナリとして配布する | 上書き | ADR-0015 | バイナリ名は`dagq`。配布経路はADR-0030がcrates.ioを足した（追加で矛盾しない） |
| 箇条1 Claude Code / Codexのpluginがskill・hookからバイナリを呼ぶ | 有効 | — | 今あるのは`plugins/claude-dagq`だけで、Codexのpluginは未着手（決定は変わっていない） |
| 箇条2 共通repoから各ecosystem向けのmanifestとpackageを出す | 有効 | — | |

### ADR-0006 repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`に置く、hashの規則、`repository`ファイル | 上書き | ADR-0015（データディレクトリ行） | ディレクトリ名は`dagq`。hashと`repository`ファイルは有効 |
| 箇条2 run dir・worktree・logを`runs/<run-id>/`に置く | 有効 | — | ADR-0017が読むたびに解決する規則を足した |
| 箇条3 cwdの`git rev-parse --git-common-dir`で解決、`--db`はoverride、`--repo`は任意 | 有効 | — | |
| 箇条4 `init`で束縛し、以後の全コマンドがopen直後に検査する | 上書き | ADR-0020 決定1 | `rebind`だけは検査を通らず束縛を付け替える |
| 箇条5 `locate`、launcherは`CMUX_TASKQ_DB`のときだけ`--db` | 上書き | ADR-0015（環境変数行） | `DAGQ_DB`。`locate`は有効 |

### ADR-0007 leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 `run_leases`でrun単位のlease、supervisorごとのtoken | 有効 | — | migration 0005の記述は記録 |
| 箇条2 claimとlease作成を同じトランザクションで行う、複数supervisorでも直列 | 有効 | — | |
| 箇条3 `supervise --parallel N`の常駐ループ、`--once`、signal | 有効（一部上書き） | ADR-0027 決定1 | 「`awaiting_integration`になったrunのleaseを解放する」は、reviewの後までleaseとslotを持つに変わった |
| 箇条4 1 runのruntime error（exit要求のtimeoutを含む）はそのrunだけをabandonしleaseを削除する | 上書き | ADR-0019 決定2 | `exit_request_timed_out`ではleaseを手放さない。ほかのruntime errorのabandonは有効 |
| 箇条5 provisioningの失敗でclaimを止めdrainして非0で終わる | 有効 | — | |
| 箇条6 `doctor` / `recover`はrunごとに判定する | 有効 | — | ADR-0041 決定3でwrapperの死んだrunの`recover`はsupervisorが自動で行うようになったが、判定の単位は変わらない |

### ADR-0008 merge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashして着地させる

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 着地は`integrate ID` / `--next`。承認制でSVがレビュー後に呼び、承認なしの自動着地とpushはruntimeが行わない | 上書き | ADR-0040 決定2（ADR-0023 決定2）、ADR-0019 決定3 | review jobのpassでsupervisorが着地させ、`integrate`がpushする。`integrate ID` / `--next`の入口は有効 |
| 箇条2 統合slotは1つ、`one_integrating_run_per_queue`、staleなら`recover`で`awaiting_integration`へ | 有効 | — | |
| 箇条3 着地の手順（receipt検査 → rebase → 検証 → `commit-tree` → ref → main → 後始末） | 有効（一部上書き） | ADR-0015（branch `taskq/` → `dagq/`、ref `refs/taskq/` → `refs/dagq/`） | ADR-0017 決定3のrepair、ADR-0029 決定4のscope検査が足された（追加）。rebase後の検証はADR-0040 決定1と整合 |
| 箇条4 mainの進め方（`merge --ff-only`か`update-ref`） | 有効 | — | |
| 箇条5 commit messageのtrailer `Taskq-Task` / `Taskq-Run` | 上書き | ADR-0015（trailer行） | `Dagq-Task` / `Dagq-Run`。title・summaryの段落は有効 |
| 箇条6 衝突と再検証の失敗は`needs_session`。SVが`claude --resume`で開き直し、`integrate ID`で再開する | 上書き | ADR-0019 決定1 | `needs_session`にすることは有効。resumeはsupervisorが自動で行う |
| 箇条7 `failed` receiptはrunの終了、再試行や取り消しは手動 | 有効（一部上書き） | ADR-0041 決定3（ADR-0024 決定3） | runを`failed`にすることは有効。再試行・resume・人への相談はtriage jobのverdictで決まる |
| 箇条8 mainを進める前のエラーは`integration_error`で元に戻す | 有効 | — | |

### ADR-0009 goalとして表現し、workerのpromptに流す

goal 1で実装済みなので、この棚卸しで`status: accepted`にした（`accepted_on: 2026-09-25`）。下の表のとおり上書きされた決定を含むので、「`accepted`のADRは本文の決定がすべて有効」（ADR-0042）をまだ満たさない。組Fの統合ADRが`accepted`になって丸ごと置き換えたときに解消する。

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 箇条1 `Goal`エンティティ、`Task.goal_id`はnullable | 有効 | — | |
| 箇条2 goalは状態機械を持たない、closeは1回のイベント、verdictの拒否条件 | 上書き | ADR-0041 決定5（ADR-0024 決定5） | draft状態を持つ。close時の拒否条件はADR-0041 決定8が`submitted`を含めて引き継ぐ |
| 箇条3 goalにverification_commandsを持たせない | 有効 | — | |
| 箇条4 `goal edit`と`goal_updated` | 有効 | — | |
| 箇条5 `set-goal`は`draft` / `ready`だけ | 上書き（未実装） | ADR-0041 決定9 | 「`ready`のtaskは編集しない」の規則がgoalの所属にも及ぶと読む（ADR-0029 決定2の`paths`と同じ扱い）。決定9が挙げる中身の一覧にgoalの所属は無いので、統合ADR（組F）を書くときに人と確かめる。実装は今も[domain-model](../design/domain-model.md)のとおりdraft / ready |
| 箇条6 依存はgoalをまたいでよい | 有効 | — | ADR-0038がgoalへの依存を足した（追加） |
| 箇条7 promptにgoal・依存元・`in_progress`の兄弟を載せる | 有効 | — | ADR-0038 決定5が依存先goalの成果を足した（追加） |
| 箇条8 `Task.context` | 有効 | — | |
| 箇条9 receiptの`follow_ups`は形だけ確認し、SVが`show`で見て登録を判断する | 上書き | ADR-0019 決定4、ADR-0041 決定16 | `integrate`がdraftに登録し、runtimeが立てるplannerが採否を決める |
| 箇条10 `candidates`はID順のまま | 上書き | ADR-0040 決定4（ADR-0023 決定4） | 効く優先度 → goalのrank → 解放数 → ID |
| 箇条11 schema v6、表とイベント | 記録 | — | イベント名は有効。schemaの番号は記録 |
| 箇条12 pluginの`taskq` skillが「課題を聞く → goal → task」を標準手順にする | 上書き | ADR-0015（skill行）、ADR-0041 決定1 | skillは`dagq`、手順の主体はplanner |
| 箇条13 着手の順と最初のgoalの4 task | 記録 | — | 完了 |
| 箇条14 journalテンプレートの節名を019で変える | 失効 | ADR-0036 決定1 | journalは削除された |

### ADR-0010 役割名の統一とlaunchd常駐、up / down

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 役割名 supervisor / maintainer / worker、SVとoperatorはmaintainer | 上書き | ADR-0041 決定1（ADR-0024 決定1） | 役割は5つでmaintainerは退役。`supervise`・`supervisors`表を変えないことは有効 |
| 2 cold startは`up`、supervisorはLaunchAgent、`up`がmaintainer workspaceを作る、`CMUX_TASKQ_ROLE` / `QUEUE` | 上書き | ADR-0011 決定1（socket password前提）、ADR-0041 決定6（`up`はsupervisorとinboxだけ）、ADR-0015（環境変数名）、ADR-0026 決定2（`--env`） | launchd常駐と`up`の1コマンドは有効 |
| 3 `down`（bootout・drain、`--wait`、`--force`）、maintainer workspaceは閉じない | 有効（読み替え） | — | `down`はinboxとplannerを閉じない（AGENTS.md） |
| 4 `up`はPIDの死んだ`supervisors`登録を消す（`up`だけの例外） | 有効 | — | ADR-0045 決定16も生きているが黙った登録はpruneしないとしており整合 |
| 5 workspace名 `taskq <repo> maintainer` / `taskq <repo> <task-id> <run-id>` | 上書き | ADR-0011 決定3、ADR-0015、ADR-0018 決定1、ADR-0021 決定1、ADR-0028 決定1 | 今は`[<repo>]<role>` |
| 6 supervisorのlogを起動ごとに`<queue dir>/logs/supervisor-<started_at>.log`へ | 失効 | ADRなし（task 194、proposedの[ADR-0033](../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)の実装） | 今は`logs/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`（[supervisor-lifecycle](../design/supervisor-lifecycle.md)）。queue dirの`logs/`に置くことと`locate`のlog dirは有効。下の「ADRの外で変わった実装」 |
| 7 maintainerの初期promptはruntime生成、CLIの使い方はskill `taskq-maintain`、AGENTS.mdはrepository固有の注意だけ | 上書き | ADR-0041 決定1、ADR-0015（skill名）、ADR-0016 決定8、ADR-0022 | 初期promptは`inbox_prompt` / `planner_prompt`。「使い方はskill、AGENTS.mdは固有の注意」は有効 |
| 末尾 taskへの分割（T2・T3） | 記録 | — | 完了 |

### ADR-0011 cmuxのsocket passwordとup --in-cmux

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 launchd modeはsocket passwordを前提にし、`up`が`CMUX_*`を除いた`cmux ping`でpreflightする | 有効 | — | AGENTS.mdの起動と停止 |
| 2 `up --in-cmux`のfallback、`down`はSIGINTでdrainしworkspaceを閉じる、modeを登録に記録する。maintainer workspaceの作成はlaunchdと同じ | 有効（一部上書き） | ADR-0041 決定6、ADR-0015（`cmux-taskq supervise`） | maintainer workspaceは作らない。自動再起動が無いことは有効 |
| 3 supervisorのworkspace名 `taskq <repo> supervisor` | 上書き | ADR-0015、ADR-0021 決定1、ADR-0028 決定1 | `[<repo>]supervisor` |

### ADR-0012

置き換え済み（→ [ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)、2026-09-25）。棚卸しの間に着地したtask 259が、段落2の自動`recover`（ADR-0041 決定3）を含む生きている決定を引き継いで丸ごと置き換えた。

### ADR-0013 レイヤーと「型＋関数」

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1〜8 レイヤー、型＋関数、集約、VO、作成と復元、エラー、時刻・ID、トリレンマ | 有効 | — | overviewのレイヤー |
| 適用: モジュール境界 | 有効 | — | moduleは後から増えた（`domain::proposal`・`stall`など、追加） |
| 適用: 状態遷移はdomainに置く | 有効 | — | |
| 適用: 外部公開API（schema v8、CLIのJSON・エラー文・exit code、`plugins/`）を変えない | 失効 | ADR-0014 決定1（→ ADR-0045）、ADR-0019、ADR-0022 決定1、ADR-0026 決定1 ほか | リファクタリングのgoalの間の制約。以後のADRがmigrationを足し、CLIとpluginも変わった |
| 適用: 進め方（1 task 1観点） | 記録 | — | goalは完了 |
| 棚卸し | 記録 | — | 着手時点のスナップショット（ADR自身がそう書く） |

### ADR-0014

置き換え済み（→ ADR-0045、2026-09-25）。

### ADR-0015 cmux-taskqをdagqに改名する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 対応表（crate、repository、archive、plugin、marketplace、launcher、環境変数、データディレクトリ、launchd label、branch、ref、trailer） | 有効 | — | |
| 1 対応表のcmux workspace名の行 | 上書き | ADR-0018 決定1、ADR-0021 決定1、ADR-0028 決定1 | |
| 1 対応表のskillの行（`dagq-maintain`） | 上書き | ADR-0016 決定8、ADR-0022、ADR-0041 決定1 | 今のskillは`dagq` / `dagq-inbox` / `dagq-planner` / `dagq-recover` |
| 1 対応表のversionの行（0.2.0） | 記録 | — | versionの付け方はADR-0045 決定1 |
| 2 文言の置き換え | 有効 | — | |
| 3 schemaとAPPLICATION_IDを変えない、versionを0.2.0に | 記録 | — | 一度きり |
| 4 互換shimを作らない | 有効 | — | |
| 5 `docs/journal/`と既存ADRは凍結、旧名はADR-0015だけ | 失効 | ADR-0036 決定1（journal削除）、ADR-0042（ADRの置き換え規則） | 既存ADRの本文を変えないことはADR-0042 決定6で有効 |
| Consequences 1 切り替え手順の4（DBを直接触る例外） | 失効 | ADR-0020 決定7 | 例外は無くなった |

### ADR-0016 status / watch / doctorの通知経路と圧縮出力、起き直しhook

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 原則 maintainerは使い捨て、runtimeは起き直しに要る情報を上限のある大きさで返す | 有効（読み替え） | ADR-0041 決定1 | 対象はinboxとplanner |
| 1 情報源は`status` / `watch` / `doctor`、`watch --after <cursor>` | 有効 | — | `--role`はADR-0022 決定1が足した |
| 2 run_eventsのkind名は公開契約、attentionの判定はdomain、attentionの一覧 | 有効（一部上書き） | ADR-0040 決定2、ADR-0041 決定3・17 | `awaiting_integration`はreview job、`failed`はtriage jobに回り、attentionにならない。一覧はADR-0022・0025・0041が足した |
| 3 runtimeはmaintainerのterminalに打ち込まない | 上書き | ADR-0041 決定1・12・13 | maintainerは無い。plannerにはrevise・answerを送る |
| 4 attentionのたびにmaintainer workspaceへ`cmux notify` | 上書き | ADR-0022 決定5 | `ask_opened`のときだけinbox宛て |
| 5 `integrate`は自動で呼ばれない、着地の承認はユーザーに残す | 上書き | ADR-0022 決定3、ADR-0040 決定2 | review jobのpassでsupervisorが着地させる |
| 6 既定は圧縮、`--full`でopt-in | 有効 | — | |
| 7 `review ID`が`review.md`を書く | 有効 | — | review jobの入力（ADR-0040 決定2） |
| 8 SessionStart hookは`DAGQ_ROLE=maintainer`のときだけ`status`、`dagq-maintain`の分割、`maintainer_prompt`の縮約 | 上書き | ADR-0041 決定1・6 | hookは`status --role <role>`（inbox / planner）。skillはinbox / planner / recoverに分かれた |

### ADR-0017 runのpathをqueueディレクトリから解決する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 queue配下のpathはrun IDとqueueディレクトリから解決する | 有効 | — | |
| 2 列と書き込みは残し、schemaは変えない | 有効 | — | |
| 3 `integrate`が`git worktree repair`する | 有効 | — | |
| 4 移動の手順 | 有効 | — | |

全決定が有効。同じ主題のADR-0006と束ねる（組B）。

### ADR-0018 runのworkspace名はtaskのtitle

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 `[<repo>]dagq#<task-id> <task title>` | 上書き | ADR-0028 決定1 | `[<repo>]worker#<task-id> - <task title>` |
| 2 run IDを`--description "run <run-id>"`に置く | 上書き | ADR-0026 決定3 | `dagq role=… queue=… run=… task=…` |
| 3 maintainer / supervisor / resumeの名前は変えない | 上書き | ADR-0021 決定1・2、ADR-0028 決定1・2 | |
| 4 `WorkspaceBackend::create`がtaskを受け取り、名前はadapterの純粋関数 | 有効 | — | |

### ADR-0019 maintainerの定型作業をruntimeに移す

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 原則 判断を含まない手順はruntime、ADR-0016の決定3・5を維持する | 上書き | ADR-0040 決定2 | ADR-0016 決定5（自動では着地しない）は維持されていない。「判断を含まない手順はruntime」は有効 |
| 1 `needs_session`のsupervisorによるresume、`integration_approved`、3回まで | 有効（一部上書き） | ADR-0040 決定2、ADR-0027 決定3、ADR-0041 決定1・3 | 「未承認のrunは`awaiting_integration`に戻しwatchで承認を待つ」はreview jobに変わった。3回で解消しないrunは`resume session`のattentionではなく`failed`にして`decide`のask（task 100、overview） |
| 2 `exit_request_timed_out`でleaseを手放さない | 有効（一部上書き） | ADRなし（task 104） | 「attention（`send /exit`）」は`stuck_exit`のaskになった（[supervisor-lifecycle](../design/supervisor-lifecycle.md)）。下の「ADRの外で変わった実装」 |
| 3 `integrate`が着地後にpushする、`push_failed`はattention | 有効 | — | 再試行は人（inboxが知らせる） |
| 4 `integrate`が`follow_ups`をdraftに登録する、`ready`にするかcancelするかは人の判断 | 有効（一部上書き） | ADR-0041 決定16・8 | 登録の形はADR-0041 決定16がそのまま引き継ぐ。採否はruntimeが立てるplannerが決め、`ready`にするのはplan reviewだけ |
| 5 taskの要求evidence、`evidence_missing`で`needs_session` | 有効 | — | |
| 6 prompt待ちを検知し`prompt_waiting`を記録、応答は人かmaintainer | 有効（一部上書き） | ADR-0041 決定17 | `answer_prompt`のaskでinboxが人に渡す（task 100） |

### ADR-0020 rebind

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1〜7 `rebind`、出力、記録、worktreeのrepair、走行中の拒否、手順、DBの直接操作の例外が無くなる | 有効 | — | |

全決定が有効。同じ主題のADR-0006と束ねる（組B）。

### ADR-0021 maintainer / supervisor / resumeのworkspace名

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 `[<repo>]dagq maintainer` / `[<repo>]dagq supervisor` | 上書き | ADR-0028 決定1、ADR-0041 決定1 | |
| 2 resume workspaceの名前 `[<repo>]dagq resume <run ID>` | 上書き | ADR-0028 決定2 | |
| 3 旧名を探す互換の検索は持たない | 有効 | — | ADR-0028 決定4も同じ（識別はUUIDなので互換が要らない） |
| 前提 `up`はtitleの完全一致で探す | 上書き | ADR-0026 決定1 | |
| 切替手順 | 失効 | — | 一度きり |

生きている決定は3だけで、ADR-0028 決定4が同じことを言う。下の「deprecated候補」の境界例。

### ADR-0022 ask / answer、inboxとplanner、疑義のあるときの着地、notify

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 原則 runtimeはどのsessionにも打ち込まない、例外はworkerへのanswer | 上書き | ADR-0041 決定12・13 | plannerにもrevise・answerを送る。worker宛てにはresume・revise・rebaseの依頼（ADR-0019、ADR-0027）も送る |
| 1 `asks`表、`ask` / `answer` / `asks`、一意性、`ask_opened` / `ask_answered` | 有効 | — | |
| 1 `kind`は4つ | 上書き | ADR-0041 決定4・11・13、ADR-0043 決定3 | `blocked`・`approve_plan`・`planner_question`・`stalled`が足された |
| 1 `watch --role maintainer`、`ask_answered`はmaintainer向け | 上書き | ADR-0041 決定6 | `--role maintainer`は無い。`ask_answered`はinbox |
| 2 workerは`worker_question`で聞いて止まり、answerはruntimeが送る | 有効 | — | |
| 3 着地は疑義のあるときだけ人に聞く。maintainerが`integrate`を呼ぶ | 上書き | ADR-0040 決定2 | 疑義の観点は有効。判断はreview job、concernの`approve_landing`のanswerはsupervisorが適用する |
| 4 `up`がinboxとplannerを開く、各役割の定義 | 上書き | ADR-0041 決定1・6 | `up`はinboxだけ（とsupervisor）。inboxの定義は有効 |
| 5 `cmux notify`は`ask_opened`のときだけinbox宛て | 有効 | — | ADR-0043 決定3が参照する |

### ADR-0023・ADR-0024

置き換え済み（0023 → ADR-0040、0024 → ADR-0041、どちらも2026-09-25）。

### ADR-0025 leaseの無い未完了runを`recover run`のattentionにする

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 leaseの無い未完了runを`recover run`のattentionにする | 有効 | — | |
| 2 attentionは`lease_released: true`の`runtime_error`だけ | 有効 | — | |
| 3 通知は足さない、maintainerは`watch`で受ける | 有効（一部上書き） | ADR-0041 決定17 | attentionはinboxが受ける |
| 4 maintainerが`doctor`を見て`recover`する | 上書き | ADR-0041 決定17 | inboxが人に知らせ、人の指示で`dagq-recover`の手順を行う |

### ADR-0026 workspaceをUUID・`--env`・workspace groupで扱う

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 識別の正はqueue DB、`session_workspaces`の行（`maintainer` / `supervisor`、後で`planner` / `inbox`） | 有効（一部上書き） | ADR-0041 決定1・6 | `maintainer`と`planner`の行は`up`が忘れる。plannerはオンデマンドで開く |
| 2 roleとqueueを`--env`で渡す、`SessionRole`は`maintainer`を含む5値 | 有効（一部上書き） | ADR-0041 決定1・2 | `maintainer`は無く、jobの`observer` / `reviewer`がある |
| 3 descriptionは機械可読の1行 | 有効 | — | |
| 4 queueごとのworkspace group | 有効 | — | |
| 5 queue hash | 有効 | — | |
| 6 titleの文字列は変えない | 失効 | ADR-0028 決定1 | |
| 切替手順 | 失効 | — | 一度きり |

### ADR-0027 workerのsessionをreviewの後まで残し、revise、merge-treeの事前判定

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 sessionを閉じる時点をreviewの後へ | 有効 | — | |
| 2 verdictを`pass` / `revise` / `concern`、reviseは2回まで | 有効 | — | |
| 3 自動resumeのsessionも同じ扱い、`integration_approved`のrunはreviewを待たない | 有効（読み替え） | — | 「maintainerか人が`integrate`を呼び済み」は人が呼んだ場合として読む |
| 4 passのとき`git merge-tree`で事前判定する | 有効 | — | |

全決定が有効。着地の経路を1本で読めるように組Dに束ねる。

### ADR-0028 titleを`[<repo>]<role>`にする

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 supervisor / worker / inboxのtitle | 有効 | — | |
| 1 maintainerのtitle | 上書き | ADR-0041 決定1 | 役割が無い |
| 1 plannerのtitle `[<repo>]planner` | 上書き | ADR-0041 決定6 | `[<repo>]planner#<planner-id>`（overview） |
| 2 resumeのworkspaceもworkerと同じtitle、description `run <run-id> resume` | 有効（読み替え） | — | 開くのはruntime（自動resume）。「当面はmaintainerが開き」は失効 |
| 3 `DAGQ_ROLE`の値、`MAINTAINER_ROLE`などの定数 | 上書き | ADR-0041 決定1・2 | `maintainer`は無い |
| 4 旧名との互換は持たない | 有効 | — | |

### ADR-0029 taskが変更してよいパスを宣言する

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1 `add --paths`とglobの規則 | 有効 | — | |
| 2 draft / readyのtaskの`--paths`を`set-paths`で置き換える | 上書き（未実装） | ADR-0041 決定9 | 中身（`paths`を含む）を編集できるのは`draft`と`submitted`だけ。readyのtaskは`submitted`に戻して直す（決定14）。AGENTS.mdとdagq skillは今も「draft / readyのうちは`set-paths`」と書く |
| 3 validatingのscope検査と`scope_violation` | 有効 | — | ADR-0040 決定1が参照する |
| 4 integrateのscope検査 | 有効 | — | |
| 5 scope違反のresume、必要なら`failed`で書かせ人が`--paths`を広げて再登録 | 有効（読み替え） | — | 再登録はplannerがproposalで行う（ADR-0041 決定1・8） |
| 6 変更の種類ごとの`--verify`と`--paths`の組み合わせ | 有効 | — | AGENTS.mdのテストの制約 |

### ADR-0030 crates.ioへのTrusted Publishing

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1〜5 追加の経路、tag pushでpublish、Trusted Publishing、第三者actionの例外、導入手順 | 有効 | — | `release` skill |

全決定が有効。統合の対象にしない。

### ADR-0031 inbox / plannerの色・pill・ピン、閉じる前のunpin

| 決定 | 現在 | 上書き | 根拠 |
| --- | --- | --- | --- |
| 1〜4 `up`がinboxに色・pill・ピンを当て、毎回当て直す | 有効 | — | |
| 1〜4 `up`がplannerに色・pill・ピンを当てる | 上書き | ADR-0041 決定6 | `up`はplannerを開かない。plannerの色とpillは`plan`が当て、ピンは付けない（[supervisor-lifecycle](../design/supervisor-lifecycle.md)、task 277） |
| 5 失敗はwarning | 有効 | — | |
| 6 閉じる前にunpin | 有効 | — | |

## 統合ADRの組

上書きされた決定を1つでも含むADRは、0001・0002・0003・0005・0006・0007・0008・0009・0010・0011・0013・0015・0016・0018・0019・0021・0022・0025・0026・0028・0029・0031の22本（0012も含んでいたが、ADR-0039が置き換え済み）。全決定が有効なのは0004・0017・0020・0027・0030の5本で、そのうち0017・0020・0027は同じ主題の組に束ね、0004・0030は置き換えない。

組は主題ごとに分け、1本が1 sessionで書ける大きさ（引き継ぐ決定がおおむね20以下）にした。どの組も他の組の完了を待たない。統合ADRは次を守る（[ADR-0042](../adr/0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)）。

- 置き換えるADRの生きている決定をすべて書き直して持ち、上書きされた決定は今の形（後のADRの決定）で書く。maintainerなどの旧称を使わない。
- 後のADR（0036〜0045）が「ADR-XXXXの決定N」として参照する決定は、統合ADRのどの決定に移ったかの対応表を本文に置く。読み手が古い番号から辿れるようにするため。
- 他の`accepted`のADRが既に持つ決定（ADR-0040・0041・0045など）は繰り返さず参照する。
- ADRの外で変わった実装（下の節）は、統合ADRが決定として書くか、人に確かめてから書く。

| 組 | 置き換えるADR | 推奨するtitle |
| --- | --- | --- |
| A | 0001、0002、0005、0015 | runtimeをRustの単一バイナリ`dagq`とpluginで配り、cmuxを最初のworkspace backendにする |
| B | 0006、0017、0020 | repositoryごとのqueueをデータディレクトリに置き、runのpathをqueueから解決し、repositoryとqueueの移動を`rebind`で扱う |
| C | 0003、0007、0025 | supervisorがrun単位のleaseでrunのlifecycleを所有して並列に実行し、死んだsupervisorのrunを引き継ぎ、手放したrunを`recover`に回す |
| D | 0008、0019、0027 | merge queueがrebase・検証・squashで着地させ、resume・push・follow_ups・evidence・prompt待ちとreview後の着地をruntimeが行う |
| E | 0029 | taskが変えてよいパスを宣言し、validatingとintegrateが宣言外の変更を止め、verificationを変更の種類で軽くする |
| F | 0009 | 複数のtaskが解く課題をgoalにし、workerのpromptに課題・依存元・兄弟を流す |
| G | 0010、0011 | supervisorをlaunchdかin-cmuxで常駐させ、`up` / `down`で起動・停止する |
| H | 0016、0022 | 状態は圧縮した`status` / `watch` / `doctor`で届け、人の判断はqueueのask / answerでinboxに届ける |
| I | 0018、0021、0026、0028、0031 | cmux workspaceをqueue DBのUUIDと`--env`で識別し、titleを`[<repo>]<role>`にし、inboxの見た目を当て、閉じる前にunpinする |
| J | 0013 | domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する |

### A: 基盤・名前・配布（0001、0002、0005、0015）

引き継ぐ決定:

- runtimeとsupervisorをRustで実装し、単一のバイナリ`dagq`として配る。domain / application / infrastructureを分ける（詳細は組J）（0001）。
- cmuxを必須のworkspace backendにし、domain / applicationはportを介して使う（0002）。
- runtimeをバイナリ、agent integrationをplugin（skill・hookからバイナリを呼ぶ）として配り、共通repoからmanifestとpackageを出す。追加の経路はADR-0030（0005）。
- 名前の対応表（workspace名・skill・versionの行を除き、今の値で書く。workspace名は組I、skillは今の`dagq` / `dagq-inbox` / `dagq-planner` / `dagq-recover`、versionの付け方はADR-0045 決定1を参照）、文言の置き換え、互換shimを作らない（0015）。

書かないもの: 0015の決定3・5と切り替え手順（一度きり・失効）。

### B: queueの場所と移動（0006、0017、0020）

引き継ぐ決定:

- queueは`$XDG_DATA_HOME/dagq/<hash>/queue.db`、hashと`repository`ファイル、`runs/<run-id>/`、cwdからの解決と`--db` / `--repo`のoverride、`init`での束縛と検査（`rebind`だけが例外）、`locate`と`DAGQ_DB`（0006）。
- queue配下のpathをrun IDとqueueディレクトリから解決し、列は記録として残し、`integrate`がworktreeをrepairし、移動の手順（0017の決定1〜4）。
- `rebind`の決定1〜7（0020）。

### C: runの所有・lease・並列・引き継ぎ・recover（0003、0007、0025）

引き継ぐ決定:

- supervisorがworkspace作成・監視・完了検証・後始末を行い、agentはworkspaceを削除せずreceiptで知らせる。同じqueueに複数のsupervisorが居てもよい（0003、0007 箇条2）。
- run単位のlease、claimとleaseの同時作成、常駐ループ（leaseとslotはreviewの後まで持つ、ADR-0027 決定1）、runtime errorのabandon（`exit_request_timed_out`は除く、組D）、provisioningの失敗、runごとの`doctor` / `recover`（0007）。
- 引き継ぎ（adopt）と自分のtokenのstaleなleaseの更新はADR-0039（ADR-0012を置き換え済み）を参照し、繰り返さない。0039も束ねて1本にするかは統合ADRを書くtaskで決めてよい。wrapperの死んだrunの自動`recover`はADR-0041 決定3、exec の引き継ぎはADR-0045 決定10を参照する。
- leaseの無い未完了runの`recover run`のattention（inbox宛て）と、その判定（0025の決定1・2）。

### D: 着地の経路（0008、0019、0027）

引き継ぐ決定:

- `integrate ID` / `--next`、単一の着地slot、着地の手順（`dagq/<run-id>`、`refs/dagq/runs/<run-id>`、repair・scope検査を含む）、mainの進め方、commit messageと`Dagq-Task` / `Dagq-Run`、衝突と検証失敗の`needs_session`、`failed` receipt（その後はtriage job、ADR-0041 決定3）、mainを進める前のエラー（0008）。reviewとpassでの着地・rebase後に必ず1回の検証はADR-0040 決定1・2を参照する。
- 判断を含まない手順はruntimeが行う原則、`needs_session`の自動resumeと`integration_approved`と3回の上限（超えたら`failed`にして`decide`のask）、`exit_request_timed_out`でleaseを手放さない（attentionは`stuck_exit`のask）、push、`follow_ups`のdraft登録（採否はADR-0041 決定16のplanner）、要求evidence、prompt待ちの検知と`answer_prompt`のask（0019）。
- sessionをreviewの後まで残す、`pass` / `revise` / `concern`、自動resumeのsessionも同じ扱い、merge-treeの事前判定（0027の決定1〜4）。

### E: taskの宣言パス（0029）

引き継ぐ決定: 0029の決定1・3〜6。決定2は、`paths`を編集できるのは`draft`と`submitted`のあいだだけ（ADR-0041 決定9）に書き直す。readyのtaskの`paths`を変えるときはADR-0041 決定14の手順。決定5の再登録はplannerのproposalで行う。

この組の統合ADRが`accepted`になると`set-paths`のreadyへの適用が決定と食い違う。AGENTS.mdとdagq skillの記述と、`set-paths`の実装をADR-0041 決定9に合わせるtaskはgoal 29の後続taskが持つ（統合ADRを書くtaskでは実装を変えない）。

### F: goal（0009）

引き継ぐ決定: 0009の箇条1・3・4・6〜8。箇条5は`set-goal`をADR-0041 決定9に合わせるか（`draft` / `submitted`だけ）を人と確かめて書く。箇条2はdraft状態を持つ形（ADR-0041 決定5）、箇条9は`integrate`のdraft登録とplannerの採否（ADR-0019 決定4、ADR-0041 決定16）、箇条10はclaim順（ADR-0040 決定4）、箇条12は`dagq` skillとplannerの手順として書く。goalへの依存（ADR-0038）は参照する。0038も束ねて1本にするかは統合ADRを書くtaskで決めてよい（0038は全決定が有効）。

### G: 起動と停止（0010、0011）

引き継ぐ決定:

- `supervise`・`supervisors`表の名前、`up`の1コマンドのcold start、supervisorのlaunchd常駐（socket passwordのpreflight、`CMUX_SOCKET_PASSWORD`の扱い）と`up --in-cmux`のfallback（自動再起動なし、`down`のSIGINTとworkspaceのclose、modeの記録）（0010の決定2・3、0011の決定1・2）。
- `up`が開くのはsupervisor（in-cmux）とinboxで、plannerは`dagq plan`（ADR-0041 決定6を参照）。
- `up`だけがPIDの死んだ登録を消す（0010 決定4）。
- supervisorのlogはqueue dirの`logs/`に置き、`locate`がlog dirを返す（0010 決定6。ファイル名と書式は下の「ADRの外で変わった実装」の1）。
- CLIの使い方はpluginのskill、AGENTS.mdはrepository固有の注意だけ、常駐sessionの初期promptはruntimeが生成する（0010 決定7を今の役割で）。
- 役割の定義はADR-0041 決定1を参照し、0010 決定1は書かない。

### H: 人への届け方（0016、0022）

引き継ぐ決定:

- 常駐session（inbox）と人が開いたplannerは状態を持たず、`status` / `watch` / `doctor`で起き直す。`watch --after`と`--role`、kind名の公開契約とattentionのdomainでの判定（今のattentionの一覧で）、既定の圧縮と`--full`、`review ID`の`review.md`、SessionStart hookの`status --role <role>`（0016）。
- `asks`表とCLI、一意性、kindの一覧（今の値で。ADR-0041・0043が足したものを含む）、`worker_question`とanswerの配送、runtimeがsessionに送るものの一覧（workerへの`/exit`・依頼・answer、plannerへのrevise・answer）、着地の疑義の観点（判断と適用はADR-0040 決定2）、`cmux notify`は`ask_opened`のときだけinbox宛て（0022）。

### I: workspaceの名前・識別・見た目（0018、0021、0026、0028、0031）

引き継ぐ決定:

- 識別の正はqueue DBのUUID（`runs.workspace_id`・`supervisors.workspace_id`・`session_workspaces`の今の行）、`--env`の`DAGQ_ROLE` / `DAGQ_QUEUE`と今の役割の値、機械可読のdescription、queueごとのworkspace group、queue hash（0026の決定1〜5）。
- titleは`[<repo>]<role>`: `[<repo>]supervisor`、`[<repo>]worker#<task-id> - <task title>`（resumeも同じ）、`[<repo>]inbox`、`[<repo>]planner#<planner-id>`（0028、ADR-0041 決定6）。名前とdescriptionはadapterの純粋関数が組み立てる（0018 決定4）。旧名との互換は持たない（0021 決定3、0028 決定4）。
- `up`がinboxに色・pill・ピンを当て直し、`plan`がplannerに色とpillを当てる。失敗はwarning。dagqが閉じる経路はunpinしてから閉じる（0031）。

0021を統合ADRで置き換えるか、`deprecated`にするかは下の候補の判断で決まる。

### J: レイヤーと「型＋関数」（0013）

引き継ぐ決定: 0013の決定1〜8、モジュール境界（今のmoduleで）、状態遷移をdomainに置く規則、DDDのトリレンマの例外（循環検出のSQL、ADR-0038 決定3が同じ例外を使う）。「外部公開APIを変えない」の固定、進め方、棚卸しは書かない。

## deprecated候補

後継が無く丸ごと無効なADRは、0001〜0034には無い。どのADRにも、統合ADRが引き継ぐべき生きている決定か、上書きした後継がある。

境界例が1つある。

- **ADR-0021**: 決定1・2と前提はADR-0028・0026に上書きされ、生きている決定3（互換の検索を持たない）はADR-0028 決定4と同じことを言う。組Iに入れて`superseded`にすれば、旧名`[<repo>]dagq maintainer`などから今の名前へ1本で辿れる。引き継ぐ決定が無いので、組Iに入れず`deprecated`（理由: 決定はADR-0028とADR-0026に上書きされ、互換の方針はADR-0028 決定4と同じ）にする選び方もある。どちらにするかは人が決める。

## ADRの外で変わった実装

決定を変えたのに、変えた`accepted`のADRが無いもの。統合ADRはこれを決定として書くか、書く前に人に確かめる。

1. **supervisorなどのlog**（ADR-0010 決定6）: task 194がproposedのADR-0033の決定1・2・7を実装し、logは`logs/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`のJSON Linesになった。ADR-0033は`proposed`のまま。組Gの統合ADRはファイル名と書式を書かずにADR-0033に任せるか、ADR-0033を先に`accepted`にするかを人と決める。
2. **exit要求のtimeout**（ADR-0019 決定2）: task 104で、attention（`send /exit`）は`stuck_exit`のaskになった。
3. **resumeの上限**（ADR-0019 決定1）とprompt待ち（決定6）: task 100（ADR-0024 決定1・6の実装）で、3回resumeして解消しない`needs_session`は`failed`にして`decide`のask、`prompt_waiting`は`answer_prompt`のaskになった。ADR-0041 決定1・17の「人に届くものはすべてinbox」の範囲だが、形（`failed`と`decide`）はADRに書かれていない。

2と3は組Dの統合ADRが決定として書く。

## 進め方

- 組ごとに1件のtaskで統合ADRを書く。各taskは、統合ADRを`status: accepted`・`accepted_on`・`supersedes`で書き、置き換えるADRを同じ変更で`status: superseded`・`superseded_by`（統合ADR）・`superseded_on`（統合ADRの`accepted_on`と同じ日）と、H1直後の注記1行にし（本文と`updated`は変えない）、[adr/README.md](../adr/README.md)の2つの表を更新する。docsだけの変更。
- deprecated候補（ADR-0021）の判断は組Iのtaskより前に人が決める。
- 全組が終わり、0001〜0034の`accepted`のADRが0004・0030（と置き換えなかったもの）だけになったら、この計画を`completed`にする。
