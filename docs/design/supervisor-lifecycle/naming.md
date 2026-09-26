---
id: design-supervisor-lifecycle-naming
type: design
title: "Naming"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0028
  - adr-0018
  - adr-0011
  - adr-0044
  - adr-0026
  - adr-0031
---

# Naming

cmux workspaceの名前は複数repositoryで同じcmuxを使うためrepository名を含み、`[<repo>]<role>`の形をとる（[ADR-0028](../../adr/0028-workspace-titles-are-repo-and-role.md)。[ADR-0018](../../adr/0018-run-workspace-named-after-the-task.md)とADR-0021の`dagq`入りの書式を上書き）。`<repo>`はrepository rootのbasename（basenameが空ならpath自体）で、`]`の直後に空白を入れない。名前の組み立ては`infrastructure::adapters`の純粋関数:

- worker: `[<repo>]worker#<task-id> - <task title>`（`run_workspace_name`。`<repo>`はrunの`repo_path`のbasename、titleはtaskのtitleを切り詰めずにそのまま）
- resume: `needs_session`のrunを`claude --resume <run-id>`で開き直すworkspaceは、workerと同じ`run_workspace_name`の名前でdescriptionを`run <run-id> resume`にする（supervisorの自動resumeが`create_resume`で開く。[`needs_session`](needs-session.md#needs_session)。人もinboxもplannerも開かない）
- supervisor（in-cmux mode）: `[<repo>]supervisor`（`supervisor_workspace_name`。[ADR-0011](../../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)の決定3）
- planner: `[<repo>]planner#<planner-id>`、runtimeがproposalのために立てたものは`[<repo>]planner#<planner-id> - proposal <proposal-id>`（`planner_workspace_name`。同時に開いている複数のplannerを見分ける。[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定6）。`plan`とruntimeが開く
- inbox: `[<repo>]inbox`（`inbox_workspace_name`）。`up`が開く

`DAGQ_ROLE`の値は`application::lifecycle`の`WORKER_ROLE` / `PLANNER_ROLE` / `INBOX_ROLE` / `OBSERVER_ROLE` / `REVIEWER_ROLE`（`SessionRole`の文字列。supervisorは`SessionRole::Supervisor`）。

titleは表示専用で、runtimeはどのworkspaceもtitleで探さない（[ADR-0026](../../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。識別の正はqueue DBのUUID: runは`runs.workspace_id`、in-cmux supervisorの登録は`supervisors.workspace_id`、inboxとin-cmux supervisorのworkspace（登録が消えても残るもの）は`session_workspaces(role, workspace_id, created_at)`（schema v11）、plannerは1つずつ`planners.workspace_id`（schema v23。plannerは作り直さず、listに居なければ`closed`と判定するだけ）。存在判定は`WorkspaceBackend::exists`（cmux adapterは`cmux --json --id-format uuids list-windows`で全windowを列挙し、windowごとの`cmux --json --id-format uuids workspace list --window <window>`をまとめたlistのidに大文字小文字を問わず一致するか）で、listに居ないUUIDの行は消して作り直す。`--window`なしの`workspace list`は呼び出し元のwindowのworkspaceしか返さないので、別のwindowへ移したinbox・supervisor・plannerのworkspaceも居るものとして扱うために全windowを見る（task 246）。1つのwindowでもlistに失敗すれば判定全体をerrorにする（見落としたworkspaceを閉じたと誤らないため）。`WorkspaceListing::list_workspaces`（`stats`）も同じ全windowのlistを読む。この文書で「`workspace list`に居る」と書くのはこの全windowのlistのこと。window一覧とwindowごとのlistの間にwindowが閉じると判定はerrorになり、その間にworkspaceが未走査のwindowから走査済みのwindowへ移ると見落とす（どちらも人の操作と重なったときだけ）。

workspaceは作成時にtitleとcommandのほかに`WorkspaceTags`を持つ（`workspace_create_arguments`が`--name`、`--description`、`--env`、`--group`、`--command`、`--focus false`の順に並べ、`--cwd`を足す）:

- **env**: `DAGQ_ROLE=<role>`と`DAGQ_QUEUE=<canonical db path>`（`application::lifecycle::session_env`）。roleは`SessionRole`（workspaceを持つのは`supervisor` / `worker` / `planner` / `inbox`）。`cmux workspace env <id> --json`で読め、workspaceの全shellに継承される。inbox session内の`up`の`skipped`判定とpluginのhookはこれを読む。plannerのworkspaceはさらに`DAGQ_PLANNER_ORIGIN=<person|runtime>`（`submit`がproposalの持ち主の種別にする）と`DAGQ_PLANNER_ID=<planner-id>`を持つ。
- **description**: `dagq role=<role> queue=<queue hash>[ run=<run-id>][ task=<id>]`（`workspace_description`）の1行。人向けの補助で、判定には使わない。
- **group**: queueのworkspace group。`WorkspaceBackend::ensure_group(queue hash, "[<repo>]")`（cmux adapterは`cmux --json --id-format uuids workspace-group create --name "[<repo>]" --external-id <queue hash>`。既にあれば同じgroupが返る）のUUIDを`--group`で渡す。`up`は最初にworkspaceを作るときに1回だけ求め、supervisorはrunのworkspaceを作るたびに求める（cmuxは最後のworkspaceが閉じたgroupを消すので、前のrunのgroup UUIDを持ち越さない）。作れなければwarning（`up`はJSONの`warnings`、supervisorはlogの`warning: cmux workspace group …`）にしてgroupなしでworkspaceを作る。cmuxは`--from`なしの`workspace-group create`でgroupのanchor workspaceを生成するので、queueごとに1つ見出しのworkspaceが増える。

workspaceを閉じる経路はすべて`WorkspaceBackend::close`を通る（supervisorのworker / resume / triage / review後の後始末、`down`と入れ替えの`close_supervisor_workspaces`）。cmux 0.64.25はピン留めしたworkspaceの`workspace close`を`Error: protected: ピン留めされたワークスペースは閉じられません。先にピンを外してください。`で拒む（確認は出さず、exit 1）ので、cmux adapterの`close`は`cmux workspace-action --action unpin --workspace <id>`を打ってから`cmux workspace close <id>`を打つ。unpinはピン留めされていないworkspaceにも成功し、失敗（workspaceが既に無いなど）してもcloseを止めない。今は`down`はinbox / plannerを閉じないが、閉じる経路が増えても同じcloseを通るのでピンが邪魔をしない（[ADR-0031](../../adr/0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md)）。`cmux workspace-group delete --close-workspaces`はピン留めしたworkspaceも閉じる（e2eのGroupGuardが使う）。

queue hashは`QueueLocation::hash()`: repositoryのqueueはqueueディレクトリ名（LaunchAgentのlabel`com.dagq.<hash>`と同じ）。`up`が`supervise`を`--db`で起動しても同じ値になるよう、`--db`のqueueでもファイル名が`queue.db`で隣に`repository`ファイルがあればディレクトリ名を使い、それ以外の`--db` queueはlabelのhash。
