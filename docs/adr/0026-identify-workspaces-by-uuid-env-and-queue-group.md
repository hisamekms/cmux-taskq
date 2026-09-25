---
id: adr-0026
type: adr
title: cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる
status: accepted
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - cmux
  - operations
related:
  - adr-0011
  - adr-0013
  - adr-0014
  - adr-0018
  - adr-0021
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# ADR-0026: cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる

## Context

runtimeはrunのworkspace（`runs.workspace_id`）とin-cmux supervisorのworkspace（`supervisors.workspace_id`）をUUIDでDBに持っている。一方で`up`は、maintainer workspaceの有無とin-cmux supervisorの「同名のworkspaceが残っている」判定を、`cmux workspace list`のtitleの完全一致（`find_named`）で行っていた（[ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)のContext）。titleは人がsidebarでrenameでき、同じtitleのworkspaceが2つあれば取り違え、同じcmuxで複数のrepositoryを流すと衝突しうる。

また`up`はmaintainerの`DAGQ_ROLE` / `DAGQ_QUEUE`をcommandの前置き（`env DAGQ_ROLE=maintainer DAGQ_QUEUE=<db> claude …`）で渡していたので、workspace自体はroleを持たない。そのworkspaceで`claude`を打ち直すと、pluginのhookもsession内の`up`もroleを判別できない。

cmux（0.64.25）がworkspaceに持てるメタデータは4つある: create時の`--env KEY=VALUE`（`workspace env <ws> --json`で読める。listには出ない。workspaceの全shellに継承される）、`--description`（listに出る自由文）、color、workspace group（`workspace-group create --external-id`で冪等に作れる）。任意のkey/valueタグは無い。

## Decision

1. **識別の正はqueue DB**。workspaceは記録したUUIDで探し、titleでは探さない。runtimeから`find_named`を無くす（`WorkspaceBackend`は`exists(workspace_id)`を持つ）。
   - runと`in_cmux`のsupervisor登録は従来どおり`runs.workspace_id` / `supervisors.workspace_id`。
   - それ以外の常駐sessionのworkspaceは新しい表`session_workspaces(role TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, created_at INTEGER NOT NULL)`（schema v11、`migrations/0011_session_workspaces.sql`）に持つ。いまの行は`maintainer`と`supervisor`（in-cmux supervisorのworkspace。登録は死ねば消えるが、workspaceはcommandが終わっても残るので、登録とは別に記録する）。後続のgoalで`planner` / `inbox`が加わる。
   - 存在判定は「DBのUUIDが`cmux --json --id-format uuids workspace list`に居るか」。居なければ行を消し、呼び出し側が作り直す。maintainerは居れば`reused`、無ければ作って行を書き`created`。in-cmux supervisorは記録したworkspaceが開いていればそのIDと`cmux workspace close <id>`を挙げて止まる（従来の同名判定と同じ扱い）。入れ替え（[ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md)）のdrain前の検査も同じ行で行う。`down`と入れ替えは登録の`workspace_id`で閉じ、閉じたものが記録と一致すれば行も消す。
2. **roleとqueueはworkspaceの`--env`で渡す**。`up`とsupervisorが作るworkspaceはどれも`--env DAGQ_ROLE=<role> --env DAGQ_QUEUE=<canonical db path>`を持つ（maintainer / supervisor / worker）。maintainerのcommandから`env DAGQ_ROLE=… DAGQ_QUEUE=…`の前置きを外す。maintainer session内の`up`の`skipped`判定は従来どおりこの2つの環境変数で行い、pluginのhookもworkspaceから継承したものを読む。roleの値は`SessionRole`（`maintainer` / `supervisor` / `worker` / `planner` / `inbox`）で、`planner` / `inbox`は値の定義だけでworkspaceはまだ作らない。
3. **descriptionは機械可読の1行**: `dagq role=<role> queue=<queue hash>[ run=<run-id>][ task=<id>]`（`workspace_description`、`src/infrastructure/adapters.rs`の純粋関数）。runのworkspaceはrunとtaskを持ち、maintainer / supervisorは持たない。判定には使わない（人向けの補助）。runのdescriptionは[ADR-0018](0018-run-workspace-named-after-the-task.md)の`run <run-id>`からこの形式に変わる。
4. **queueごとのworkspace group**。`WorkspaceBackend::ensure_group(external_id, name)`をcmux adapterは`cmux --json --id-format uuids workspace-group create --name <name> --external-id <queue hash>`で実装し（既にあれば同じgroupが返る）、workspaceは作成時に`--group <group UUID>`で入れる。external IDはqueue hash、nameは`[<repo>]`（`workspace_group_name`）。`up`はgroupを最初にworkspaceを作るときに1回だけ求め、すべてreuseした`up`はgroupに触らない。supervisorはrunのworkspaceを作るたびに求める（cmuxは最後のworkspaceが閉じたgroupを消すので、UUIDを持ち越すと消えたgroupを指しうる）。作れなければwarning（`up`のJSONの`warnings`、supervisorはlog）にしてgroupなしでworkspaceを作る。
5. **queue hash**は`QueueLocation::hash()`: repositoryのqueueはqueueディレクトリ名（labelの`com.dagq.<hash>`と同じ）。`up`は`supervise`を`--db`で起動するので、`--db`で開いたqueueでも、ファイル名が`queue.db`で隣に`prepare`が書く`repository`ファイルがあればディレクトリの正規化した名前を使い、同じqueueに同じexternal IDを与える。それ以外の`--db` queueはlabelのhash（canonical pathのhash）。
6. titleの文字列はこのADRでは変えない（`[<repo>]dagq maintainer`などのまま。表示専用になったので、後続taskで`[<repo>]<role>`形式に改める）。

このADRは[ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)の「`up`はtitleの完全一致で探す」前提と、[ADR-0018](0018-run-workspace-named-after-the-task.md)のrun descriptionの書式を上書きする。既存ADRは書き換えない。

### 切替手順

新しいバイナリの`up`は`session_workspaces`の行でしかmaintainer workspaceを見つけない。旧バイナリが開いたmaintainer workspaceには行が無い。

- **maintainer**: 固定バイナリを入れ替えた後、maintainer sessionの**中**から`up`を打つ分には`skipped`なので二重にならない（旧maintainerのclaudeはcommandの前置きで`DAGQ_ROLE` / `DAGQ_QUEUE`を持っている）。maintainer sessionの**外**から打つと2つ目のmaintainerを作るので、入れ替える前に旧maintainer workspaceを閉じておくか、作られた方を残して古い方を閉じる。以後は行のUUIDで`reused`になる。
- **supervisor**: 必ず**旧バイナリで**`down --wait`（drainを待ち、登録の`workspace_id`でworkspaceを閉じる）→ バイナリ入れ替え → 新バイナリで`up --in-cmux`の順に行う。この切替ではversionの違うsupervisorを`up`が入れ替える経路（[ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md)）に頼らない: 新バイナリはqueueを開いた時点でschemaをv11にmigrateするので、旧supervisorと走行中のrunが動いているうちに新バイナリの`up`を打つと、それらは`unsupported queue schema version`で壊れる。crashして登録も行も無い旧supervisorのworkspaceは`up --in-cmux`を止めないので、人が画面を読んで閉じる。
- 同じ理由で、入れ替えるまで本番queueを`target/`のバイナリで開かない（AGENTS.md）。

## Alternatives

- **titleで探したまま、titleを一意にする**: renameに弱いことは変わらず、人が同じtitleのworkspaceを作れば取り違える。
- **descriptionにUUID以外の識別子を置いて探す**: descriptionも人が`set-description`で書き換えられる。DBに記録したUUIDはruntimeだけが書く。
- **`workspace env`で全workspaceを走査してroleを探す**: listに出ないのでworkspaceごとに1回呼ぶことになり、同じroleのworkspaceが2つあれば結局どちらか決められない。envは判定の補助（session内の`skipped`とhook）に留める。
- **groupを最初のworkspaceから`--from`で作る**: cmuxが生成するanchor workspaceは要らなくなるが、既にgroupがあるときの扱い（`--from`を無視するか）が契約に無く、作成と追加の2経路が要る。

## Consequences

- workspaceのtitleを人がrenameしても、`up`の`reused`判定もin-cmux supervisorの残骸検出も変わらない。同じtitleのworkspaceを人が開いても取り違えない。
- maintainer workspaceを閉じると、次の`up`は行を消して作り直す。cmuxを再起動してUUIDが変わった場合も同じ。
- `workspace list`はcmuxの呼び出し元のwindowのworkspaceしか返さない（`find_named`と同じ制約）。別windowに移したmaintainerは見つからず、2つ目が作られる。
- cmuxは`--from`なしの`workspace-group create`でgroupのanchor workspaceを生成するので、queueごとに1つ空のanchor workspaceがsidebarに増える（groupの見出しになる）。
- workspaceの`--env`は`CMUX_*`を上書きできない。`DAGQ_*`は影響を受けない。
