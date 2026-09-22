---
id: adr-0014
type: adr
title: upはbinary versionの違うsupervisorをdrainして入れ替える
status: accepted
created: 2026-09-22
updated: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - persistence
  - operations
related:
  - adr-0003
  - adr-0005
  - adr-0010
  - adr-0011
  - adr-0012
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0014: `up`はbinary versionの違うsupervisorをdrainして入れ替える

## Context

固定バイナリ`~/.local/bin/cmux-taskq`の更新は、これまで人の手順だった: `down`（できれば`--wait`）→ ファイルを置き換える → `up`。順序を間違えると壊れる。

実運用のqueueのtask 20（2026-09-22）がその事故を出した。新しいschemaを持つ`target/debug`のバイナリでqueue DBを開いたため、openしただけでmigrationが走ってDBが新しい`user_version`になり、古いschemaのまま走っていた固定バイナリのsupervisorとその配下のrunが`unsupported queue schema version`で全部止まった。以来[AGENTS.md](../../AGENTS.md)は「本番queueには固定バイナリだけを使う」「固定バイナリの更新はユーザーに報告してから」という運用ルールでこれを避けている。ルールは守れるが、更新そのものは依然として手順であり、`down`を忘れれば古いbinaryのsupervisorが新しいDBを掴んだまま走り続ける。

runtimeの側には、古いsupervisorを見分ける材料が何もなかった。`supervisors`表が持つのはtoken・pid・parallel・heartbeat・`mode`・`workspace_id`だけで、どのbuildのプロセスかは書かれていない。[ADR-0010](0010-maintainer-and-resident-supervisor.md)の`up`は「生きていてheartbeatの新しい登録があればreuse」なので、置き換えたはずの古いbinaryのsupervisorをそのまま使い続ける。[ADR-0005](0005-binary-and-plugin-distribution.md)でバイナリをGitHub Releaseで配る以上、更新は繰り返し起きる。

一方でsupervisorは勝手に止めてよいプロセスではない。[ADR-0003](0003-supervisor-owns-lifecycle.md)のとおりrunのlifecycleを所有していて、走行中のrunはcmux workspaceの中のClaude sessionである。止めるなら`down --wait`と同じdrain——claimを止め、持っているrunの完了を待ち、自分で登録を消す——でなければならない。drainは数分から数時間かかりうる。

## Decision

1. `supervisors`に`binary_version`を足す（migration `0010_supervisor_binary_version.sql`、schema v10）。書くのは登録するプロセス自身で、`register_supervisor`が自分の`CARGO_PKG_VERSION`（`cmux_taskq::VERSION`）を入れる。どのbuildで動いているかを知っているのはそのプロセスだけなので、`mode`（`up`が書く）とは書き手が違う。列を足す前のbinaryが書いた行はNULLになり、これは「この binary の version ではない」に含める。`status` / `doctor`は`supervisors[].binary_version`として出す。
2. `up`はlive（PIDが生きていてheartbeatが30秒以内）な登録の`binary_version`が自分の version と1つでも違えば、reuseせずに入れ替える。停止は`down --wait`と同じで、LaunchAgentを外し（bootoutがSIGTERMを運び、`KeepAlive`が古いbinaryを即座に再起動するのも止める）、`in_cmux`の登録にはSIGINTを、launchdが signal しなかったプロセスにはSIGTERMを送り、登録が消えるまで待ち、`in_cmux`のworkspaceを閉じてから、いつもの経路で1つ起動する。結果は`supervisor.outcome = "restarted"`に`version`・`previous_version`・`replaced`（消した登録のtoken / pid / mode / workspace_id / version）・`supervisor_workspaces`を添える。version が同じなら従来どおり`reused`。
3. drainに上限は置かない。runはClaude sessionなので、待ち時間はrunの長さそのものである。待てないときのために`up --no-wait`を用意し、走行中のrunが1件でもあれば件数とrun idを挙げたerrorで止める（何もsignalせず、plistも触らない）。走行中のrunが無ければそのまま入れ替えるが、`--no-wait`のときだけはdrainの待ちにも上限（`startup_timeout`）を置く: runが無くても止まらないsupervisorはありうる（loopがcmuxやgitの呼び出しでhangしていても、heartbeat threadは別なので登録は新しいまま残る。判定とsignalの間にrunがclaimされることもある）ので、上限が無ければ`--no-wait`が名前どおりに振る舞わない。上限を超えたらerrorで止め、既に停止を頼んだこと（agentは外れている）と`up`を打ち直せばよいことを文面に書く。
4. 起動し直すmodeは、入れ替えられるsupervisorのmodeではなく、その`up`が指定されたmode（`--in-cmux`の有無）にする。

## Alternatives

- **明示的な`up --restart`**: 入れ替えを別のフラグにし、既定は今までどおりreuse。事故の形——バイナリを置き換えたのに古いsupervisorが走り続ける——は、まさに「更新したことを`up`に伝え忘れる」ことで起きる。既定が安全側（新しいbinaryが serve する）でなければ、フラグは事故を防がない。またフラグを足しても、古いsupervisorを止める仕事は同じだけ要る。採らなかった。
- **`up`が version の違いを報告するだけで止める**: maintainerが`down --wait`を打ってから`up`をやり直す。今の手順の自動化としては一段進むが、結局2コマンドで、しかも`down`と`up`の間に別のsupervisorが立つ隙がある。入れ替えを1コマンドにするほうが、ドッグフーディングのmaintainerにも人にも短い。
- **supervisor自身にversionを見張らせる**: 常駐プロセスが自分のbinaryのmtimeやversionを見て、変わっていたらdrainして終わる。launchdの`KeepAlive`なら新しいbinaryで起動し直るが、`--in-cmux`には再起動の主体がいない（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）し、更新を決めるのは人なのに判断がプロセス側に散る。`up`はもともと「runtimeが望む状態にする」コマンドなので、そこに置く。
- **`binary_version`をqueue dirのsidecar fileに置く**: `mode`のときと同じ理由で採らない（[persistence](../design/persistence.md)のRuntime ownership）。versionは登録されたプロセスの性質なので、登録行と寿命を共にするのが正しく、fileはプロセスが死んだ後も残って独自のstale判定と後始末を要求する。
- **schema versionで判定する**: 「migrationを増やしたbuild」しか見分けられない。migrationを伴わない修正版のbinaryに入れ替えたときに古いsupervisorが残る。

## Consequences

- 固定バイナリの更新が「ファイルを置き換えて`up`」の2手になる。`up`が古いsupervisorをdrainし、走行中のrunを最後まで見届けさせてから、新しいbinaryのsupervisorを立てる。task 20の事故のうちruntimeが防げる部分——古いbinaryのsupervisorが新しいDBを掴んだまま走り続ける——はこれで消える。DB自体を古いschemaのbinaryから守るのは従来どおり「本番queueには固定バイナリだけ」の運用ルールで、こちらは変わらない。
- `up`が長く返らないことがある。走行中のrunがあれば、その完了まで待つ。これは意図した挙動で、`--no-wait`が待たないための答えになる（走行中のrunがあれば入れ替えない）。
- version を記録しないsupervisor（列より古いbinary、または手で起動した古いbinary）は、最初の`up`で必ず入れ替えられる。これは移行時に一度だけ起きる望ましい挙動で、入れ替え後はNULLの行が残らない。
- `up`が`down`の停止ロジックを共有する。両者が食い違うと、片方だけが直った停止の穴（たとえば[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)のworkspace close）が生まれるので、`tests/lifecycle.rs`は両方の経路を同じfakeで検査する。
- 1つのqueueをversionの違う複数のsupervisorが serve している状態は、次の`up`でまとめて解消される（liveな登録のうち1つでも version が違えば、全部drainして1つ立て直す）。
- **判定の対象はliveな登録だけ**、つまりPIDが生きていてheartbeatが30秒以内のものだけである。PIDは生きているがheartbeatの止まった「生きているが黙っている」supervisorは、`up`がreuseもpruneもkillもしない従来の方針（[supervisor-lifecycle](../design/supervisor-lifecycle.md#up--down)）をそのまま引き継ぐので、それが古いbinaryでも入れ替えられない。`up`はその隣に新しいversionのsupervisorを1つ立て、古いほうは動き続ける。この穴は「黙っているプロセスを`up`が勝手に殺さない」という別の決定の裏側なので、ここでは閉じない。`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直す。
- `binary_version`は`CARGO_PKG_VERSION`なので、**versionを上げずにbuildし直したbinary**（この repository のドッグフーディングでよくある）は前のものと同じ文字列を名乗り、`up`はreuseしてしまう。列が無かった頃の登録（null）からの初回だけは必ず入れ替わる。リリースをまたがない差し替えを効かせたいときは、`~/.local/bin/cmux-taskq`を置き換える前に`Cargo.toml`のversionを上げるか、`down --wait`で明示的に止める。build hashを名乗る案は、versionという語の意味が2つになるので採らなかった。
