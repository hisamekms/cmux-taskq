---
id: adr-0011
type: adr
title: launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする
status: accepted
created: 2026-09-22
updated: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0010
  - adr-0002
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0011: launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする

## Context

[ADR-0010](0010-maintainer-and-resident-supervisor.md)の決定2は、supervisorをlaunchdのLaunchAgent（`KeepAlive`）として常駐させ、cmux workspaceを持たないとした。task 15（`up` / `down`の実装、ADR-0010のT2）の実機検証で、この構成だけでは動かないことが分かった。launchdが起動したsupervisorはcmuxの中で起動されたプロセスではないので、cmux（開発環境は0.64.25）は「cmux内で起動されたプロセスのみ接続できます」とsocketへの接続を拒む。supervisorはpreflightの`cmux ping`で落ち、`supervisors`表に登録が現れず、`up`は30秒待ってerrorになり、launchdは`KeepAlive`で二度と通らない起動を繰り返す（`down`で外すまで続く）。

cmuxのSocket Authはpasswordによる認証を持つ。cmuxは`--password`、環境変数`CMUX_SOCKET_PASSWORD`、cmuxのSettingsに保存したpasswordの順に解決し、いずれかが設定されていればcmux外のプロセスからの接続を受け付ける。つまりlaunchd常駐が成り立つかどうかは、cmux側の設定という運用上の前提に依存する。ADR-0010はこの前提を書いておらず、`up`もそれを確認しないままplistを書いてbootstrapするので、前提が満たされていない環境では失敗の原因がsupervisorの`launchd.log`にしか残らない。

task 15の結果は次のように扱う。(1) task 15はこの制約をreceiptに記録したまま受け入れる。(2) 恒久的には、cmuxのsocket passwordを運用上の前提にし、`up`がpreflightで接続性を確かめて明確に失敗する（task 21）。(3) launchdを使わずcmux workspaceの中でsupervisorを起動する`up --in-cmux`をfallbackとして足す（task 22）。ADR-0010はacceptedで、既存のADRは書き換えない（[README](README.md)）ので、この決定を新しいADRとして残す。

## Decision

1. **ADR-0010の決定2の修正: launchd modeはcmuxのsocket passwordを前提にする。** launchdで常駐するsupervisorがcmuxに接続できるのは、cmuxのsocket passwordが設定されているときだけとする。cmuxのSettingsに保存したpasswordを推奨する。`CMUX_SOCKET_PASSWORD`は`up`を打ったshellがexportしているときに限って使い、その場合だけplistの`EnvironmentVariables`に載せてsupervisorへ引き渡す。それ以外の経路でpasswordをplistに書かない。`up`はplistを書く前、`launchctl bootstrap`する前に、cmux外からの接続が通ることを`cmux ping`で確かめる。このpingはplistが持つのと同じ環境（PATH、exportされていれば`CMUX_SOCKET_PASSWORD`）で、現在のcmux sessionから継承した`CMUX_*`の環境変数をすべて取り除いて実行する（cmuxの中で`up`を打つと、継承した変数のせいでcmux内プロセスとして通ってしまい、launchdからの接続性を確かめたことにならないため）。拒まれたらpreflightの失敗として止め、plistにもlaunchctlにも触らず、対処を2つ挙げたメッセージを出す: cmuxのSettingsでsocket passwordを設定する（または`CMUX_SOCKET_PASSWORD`をexportする）、もしくは`up --in-cmux`を使う。実装はtask 21。
2. **`up --in-cmux`をfallbackにする。** `--in-cmux`を与えた`up`はlaunchdを使わず、専用のcmux workspaceの中で`cmux-taskq supervise --parallel N --log-dir <queue dir>/logs`を起動する。maintainer workspaceの作成とPIDの死んだ`supervisors`登録の削除はlaunchd modeと同じ。launchdの`KeepAlive`に相当するものはなく、自動再起動はしない（supervisorが死んだら人が`up --in-cmux`を打ち直す）。`down`はこのmodeでは登録されたsupervisorのPIDにsignalを送り（SIGINT。runtimeはSIGTERMと同じくdrainに入る。`--wait`はdrainの完了を待ち、`--force`はkillする）、supervisorのworkspaceを閉じる。どちらのmodeで起動したかは`supervisors`の登録（またはqueue dir配下のsidecar file。どちらにするかはtask 22が決めてsummaryに理由を書く）に記録し、`status` / `doctor` / `down`がmodeを見て振る舞いを変える。決定1のpreflightが失敗し`--in-cmux`が与えられていないとき、errorメッセージは`--in-cmux`を提案する。実装はtask 22。
3. **ADR-0010の決定5の修正: supervisorのworkspace名。** in-cmux modeのsupervisor workspaceは`taskq <repo> supervisor`（`<repo>`はrepository rootのbasename。maintainerの`taskq <repo> maintainer`、workerの`taskq <repo> <task-id> <run-id>`と同じ規則）。task 22が前提にしている名前をここで確定する。

task 21とtask 22がこのADRを実装する。両taskの実装と設計文書（[supervisor-lifecycle](../design/supervisor-lifecycle.md)、[persistence](../design/persistence.md)）はこのADRと整合させる。

## Alternatives

- **passwordを無条件にplistへ書く**: `up`がSettingsや環境から得たpasswordをplistの`EnvironmentVariables`に常に載せれば、launchd modeは設定なしで通る。しかし`~/Library/LaunchAgents/`のplistは平文のファイルで、secretをそこへ書くことになる。shellが明示的にexportしたときだけ引き渡し、それ以外は書かない。
- **in-cmux modeだけにしてlaunchdをやめる**: cmuxの中でsupervisorを動かすなら接続の問題は起きない。しかしcmuxの終了で止まり、killされた後の再起動を人が判断する構成に戻る。OSが守る再起動（`KeepAlive`）を得ることがADR-0010でlaunchdを選んだ理由なので、launchdを既定に残し、in-cmuxはfallbackに留める。
- **cmux内のhelperがlaunchdの代わりをする**: cmux workspaceの中でhelperプロセスを動かし、そこからsupervisorを起動・再起動して接続を中継する案。helper自体を誰も守れず、cmuxの終了で一緒に止まる。ADR-0010のAlternativesで退けた自前watchdogと同じ問題になる。
- **`up`が確認せずにbootstrapし、失敗をlogに任せる（現状）**: task 15の状態。失敗の原因が`launchd.log`にしか残らず、`KeepAlive`が通らない起動を繰り返す。前提を満たしているかを`up`が先に確かめるほうが、失敗の場所と対処が明確になる。

## Consequences

- launchd modeは設定によるopt-inになる。cmuxのsocket passwordを設定していない環境では`up`はpreflightで止まり、supervisorは起動しない。初回セットアップの文書（READMEの`up`の節と[supervisor-lifecycle](../design/supervisor-lifecycle.md)）はpasswordの設定方法を書く。
- `CMUX_SOCKET_PASSWORD`をexportして`up`を打つと、plistにpasswordが載る。Settingsに保存する経路を推奨し、plistに載る条件を文書に明記する。
- in-cmux modeはcmuxと運命を共にする。cmuxが終了するとsupervisorも止まり、`supervisors`表にstaleな登録行が残る。次の`up`（どちらのmodeでも）がADR-0010の決定4に従ってその行を消す。in-cmux modeのsupervisor workspaceは`down`が閉じる。cmuxはcommandが終わってもworkspaceを閉じないので、supervisorがcrashや起動失敗で自ら終わったときはworkspaceが残り、人が閉じる。
- `supervisors`の登録（またはsidecar）にmodeが加わり、`status` / `doctor` / `down`はmodeを読む。schemaを変えるならmigrationが増え、[persistence](../design/persistence.md)を更新する。
- `up`のpreflightにcmux外からのpingが増え、cmuxの中から`up`を打っても継承した`CMUX_*`に頼らない検査になる。テストはfake cmuxで、拒否時にplistもlaunchctl呼び出しも起きないこと、環境の掃除、`--in-cmux`の提案を確かめる。
- task 21とtask 22はこのADRを実装するtaskで、決定1〜3と矛盾する変更はこのADRの後続ADRなしには入れない。ADR-0010の決定2と決定5は本ADRの範囲で修正されるが、ADR-0010自体は書き換えない。
