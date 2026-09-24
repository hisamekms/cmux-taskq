---
id: adr-0031
type: adr
title: upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる
status: accepted
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - cmux
  - operations
related:
  - adr-0026
  - adr-0028
  - design-supervisor-lifecycle
---

# ADR-0031: upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる

## Context

`up`が開く`[<repo>]inbox`と`[<repo>]planner`は常駐のworkspaceで、cmuxのsidebarではrunのworkspaceと同じ見た目のまま並ぶ。titleは[ADR-0028](0028-workspace-titles-are-repo-and-role.md)で役割名を含むが、runのworkspaceが増えると埋もれ、人がrenameすることもある（識別はUUID。[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。2026-09-24にユーザーが、inbox / plannerのworkspaceに色を付け、アイコンで役割がわかるようにし、ピン留めしたいと求めた。あわせて、supervisorや`down`の処理ではピンを外してから閉じることを求めた。

cmux 0.64.25の手段:

- `cmux workspace-action --action set-color --color <name|#RRGGBB>`（名前はRed..Charcoalの16色）、`pin` / `unpin`
- `cmux set-status <key> <value> --icon <name>`: sidebarのstatus pill。keyごとに1つで、同じkeyは上書きされる。iconはSF Symbolの名前
- workspace自体にアイコンの属性は無い

実機で確かめたこと: ピン留めしたworkspaceへの`cmux workspace close`は確認を出さずに`Error: protected: ピン留めされたワークスペースは閉じられません。先にピンを外してください。`（exit 1）で拒まれる。`unpin`はピン留めされていないworkspaceにも成功する。`workspace-group delete --close-workspaces`はピン留めしたworkspaceも閉じる。

## Decision

1. **色**: `up`はinboxを`Amber`（人の判断を待つもの）、plannerを`Blue`にする（`lifecycle::session_look`）。
2. **アイコンはstatus pillで出す**: workspaceにアイコンが無いので、`cmux set-status dagq_role <role> --icon <icon>`でpillを出す。keyはdagq固有の`dagq_role`にし、Claude Codeの`claude_code`など他のツールのpillと衝突させない。iconはinboxが`tray`、plannerが`map`（どちらもSF Symbolに在ることを確かめた）。
3. **ピン**: `up`はinbox / plannerのworkspaceを`workspace-action --action pin`でピン留めする。
4. **毎回当て直す**: 1〜3は`created`だけでなく`reused`でも、`up`がその役割のsession内から打たれて`skipped`になるときも記録したworkspaceが居れば当てる。どれも冪等なので、以前のバイナリの`up`が開いたworkspaceや人が外したピンにも付く。supervisorとrunのworkspaceには当てない。
5. **失敗はwarning**: 3つの呼び出しは独立で、失敗しても`up`は成功し、理由を`up`の`warnings`に載せる（`backend_call_failed`にも残る）。見た目のために常駐sessionの起動を止めない。
6. **閉じる前にunpin**: dagqがworkspaceを閉じる経路はすべて`WorkspaceBackend::close`を通るので、cmux adapterの`close`1か所で`workspace-action --action unpin`を打ってから`workspace close`を打つ。unpinの失敗はcloseを止めない（closeの結果が正）。今`down`はinbox / plannerを閉じないが、閉じる経路が増えてもピンで止まらない。

## Consequences

- inbox / plannerはsidebarの上（ピンの領域）に色とpillつきで並び、runのworkspaceと見分けられる。
- workspaceのcloseはcmuxの呼び出しが1回増える。unpinは`output`の期限内で終わる軽い呼び出しで、失敗しても記録しない（closeの失敗だけが`backend_call_failed`になる）。
- 人がsupervisorやrunのworkspaceをピン留めしても、dagqの後始末と`down`は閉じられる。
- dagqの外で`cmux workspace close`を打つ手順（人の後始末、e2eのguard）は、ピン留めしたworkspaceを先にunpinする必要がある。
- 色とiconはコードの定数で、設定にはしていない。変えたくなったら設定を足す判断を別に行う。
