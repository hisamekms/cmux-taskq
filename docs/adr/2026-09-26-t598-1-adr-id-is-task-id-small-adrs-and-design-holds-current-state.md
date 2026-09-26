---
id: adr-t598-1
type: adr
title: ADRのIDを書くtaskのIDにし、1 ADR 1決定・記載の粒度・今の姿はdesign・大きなADRはamendsで直すと決める（ADR-0042を置き換え）
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0042
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - docs-frontmatter
  - adr-index
  - adr-0042
---

# ADR-t598-1: ADRのIDを書くtaskのIDにし、1 ADR 1決定・記載の粒度・今の姿はdesign・大きなADRはamendsで直すと決める（ADR-0042を置き換え）

## Context

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)は、ADRを丸ごと置き換え、置き換え・廃止の日付をfrontmatterと本文冒頭の注記に残すと決めた。その上で、ADRの番号はplannerがmainの次の空きを選ぶ運用にしていた。

2026-09-26の調べで、次の問題が分かった。mainは未完了のtaskが予約した番号を知らないので、計画時に番号が衝突し、plan reviewが7回reviseで差し戻した。丸ごと置き換えと決定番号での参照（未完了のtaskの本文に多数）が重なり、置き換えるADRのtaskの依存が深くなった。ADRには一つが数百行・数十の決定を持つものがあり、eventの欄名や閾値の数値まで書かれて（例: ADR-0069の決定8・9）、実装の細部が変わるたびに丸ごとの置き換えが要る。人は2026-09-26にplannerとの対話で、ゼロベースで考えた次の決定を採った。

## Decision

1. **IDは書くtaskのIDと枝番にする。** 新しいADRのIDは`adr-t<task ID>-<N>`（Nは1から。1本でも`-1`を付ける）、ファイル名は`docs/adr/<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`で、日付は`accepted_on`（proposedで書いたものは書いた日で、acceptedにする変更でファイル名とそこへのリンクを合わせる）。参照は`ADR-t<ID>-<N>`で日付を含めないので、着地前から後続のtaskが参照できる。plannerはADRを書くtaskのdescriptionに本数と各IDの中身を書く（自分のIDはaddが返すまで分からないので「このtaskのIDで」と書くか、addの後にdraftを直す）。既存の0001〜0078と、登録済みのtaskが予約した4桁の番号はそのまま使い、振り直さない。新しく登録するADRのtaskは新しい形にする。
2. **1 ADRに決定1つ（密に結びついた数個まで）。** 本文はおおむね100行以内にする。小さければ丸ごと置き換えが安く、決定番号で参照する必要も無い。
3. **ADRには変えるのに人の判断が要るものだけを書く。** 書くのは、問題と文脈、方針・原則・境界・不変条件、退けた案と理由、結果とトレードオフ。eventのkindやpayloadの欄名、CLIのflagの綴り、JSONの形、既定値・閾値の数値、関数・モジュール・ファイル名、migrationの番号、testの名前は書かず、`docs/design/`に書く。目安は「これを変えるとき人に聞くか」。
4. **今の姿は`docs/design/`が持つ。** task・code・文書は、今どうなっているかを引くときはdesignの文書を、なぜそうしたかを引くときはADRを指す。今の決定を1か所で読むための統合ADRは作らない。
5. **置き換えは丸ごと、ただし凍結した大きなADRはamendsで直す。** ADRの決定を変えるときは、まだ生きている決定を書き直して引き継ぐ新しいADRで丸ごと置き換える（1本で複数を置き換えてよい）。ただし既存の4桁のADRのうち、決定が多く丸ごとの書き直しが1 taskに収まらないもの（例: ADR-0047・0044・0073）は凍結し、その一部を変えるときは、小さな新しいADRの`amends`に変える決定（ADRのIDと決定番号）を書き、元のADRに`amended_by`を足し、同じ変更でdesignの文書を今の姿に直す。部分的な上書きの連鎖はdesignが今の姿を持つことで読む側に残さない。
6. **statusは`proposed` / `accepted` / `rejected` / `superseded` / `deprecated`の5つ。** `accepted`のADRは本文の決定がすべて有効（`amended_by`を持つものは、その決定だけ後のADRが変えている）。置き換えの日は`superseded_on`、後継なしの廃止の日は`deprecated_on`に分け、`accepted_on`・`superseded_by`・`supersedes`と合わせた欄の組み合わせと書式は[frontmatter仕様](../frontmatter.md)に置く。
7. **置き換えは後継を`accepted`にする変更と同じ変更で行う。** 古いADRの`superseded_on`は後継の`accepted_on`と同じ日にする。`proposed`の後継は何も置き換えない。
8. **`superseded` / `deprecated`のADRはH1の直後に1行の注記を置く。** 日付は`superseded_on` / `deprecated_on`と同じにする。
9. **本文はappend-only。** 後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`と決定8の注記だけ。`supersedes`と`amends`は本文と一緒に書き、後から変えない。
10. **[docs/adr/README.md](README.md)を索引にする。** 有効なADRの一覧と、置き換え・廃止の対応表を置き、statusを変える変更で同じく更新する。新しい形の行は`accepted_on`の順に4桁の行の後ろに並べる。
11. **既存の0001〜0034の棚卸しは[ADRの棚卸し](../plans/adr-inventory.md)のまま続ける。** 日付はgit logで確かめる。組み直すかはplannerが人と決める。
12. **決定と実装が明らかなものはADRと実装を1 taskにする。** ADRだけのtaskは、書いたものを人が確かめたいときに限る。

## ADR-0042からの対応

| ADR-0042 | このADR |
| --- | --- |
| 決定1（statusは5つ） | 決定6 |
| 決定2（丸ごと置き換え、部分的なADRを書かない） | 決定5（凍結した大きなADRはamendsで直す例外を足した） |
| 決定3（frontmatterの欄と日付） | 決定6（欄の表はfrontmatter仕様に置く） |
| 決定4（後継のacceptedと同じ変更で置き換える） | 決定7 |
| 決定5（H1直後の注記） | 決定8（書式はfrontmatter仕様とtemplate） |
| 決定6（append-only） | 決定9（`amended_by`を足した） |
| 決定7（索引） | 決定10 |
| 決定8（0001〜0034の棚卸しと統合ADRでの置き換え） | 決定11（統合ADRで置き換える義務は外し、組み直すかを人と決める。ADR-0009のacceptedは済み） |
| 原則（後継を1本辿れば生きている決定が読める） | 決定5・6（amendsで直したADRは`amended_by`を辿るか、designで今の姿を読む） |

## Alternatives

- **mainの次の空き番号**: 未完了のtaskの予約を知らず、plan reviewが7回差し戻した。
- **番号を割り当てるCLI**: runtimeの変更が要り、plannerのaddの前に別の手順が増える。taskのIDはすでに一意に割り当てられている。
- **着地時の振り直し（migrationと同じ）**: ADRは後続のtaskと文書から番号で参照されるので、振り直すと参照が壊れる。
- **日付をIDに含める**: acceptedの日は計画時に分からず、後続のtaskが参照を書けない。
- **slugをIDにする**: 途中でslugを変えると参照が壊れる。
- **1本目は枝番なし**: `t598`と`t598-1`が同じかの曖昧さが残り、2本目を足すと1本目だけ形が違う。
- **英字の枝番**: 数字と比べて利点が無く、順序が読みにくい。
- **決定の多いADRも丸ごと置き換え続ける（ADR-0042のまま）**: 一部を変えるたびに数十の決定を書き直すtaskになり、依存が深くなる。
- **統合ADRで今の決定を1か所にまとめる**: designの文書と役目が重なり、二重に保つことになる。

## Consequences

- 計画時のIDの衝突が無くなり、plan reviewはIDの割り当ての棚卸しをしない。検査は形とファイル名・ID・日付の一致だけになる。
- 4桁と新しい形の2つのIDが並ぶ。検査スクリプトは両方を見る。taskとADRの関連の手がかりが新しい形を拾うようにするのは別のruntimeのtaskで行う。
- 決定の多いADRは`amended_by`を持つと全部は有効でなくなる。読み手は`amended_by`から後のADRを辿るか、designの文書で今の姿を読む。
- このADR自身は新しい形の最初のADRで、ADR-0042の決定を引き継ぐため決定が多い。ADRの書き方という1つの主題にまとまっているので分けない。
- AGENTS.md・[frontmatter仕様](../frontmatter.md)・[索引](README.md)・[template](0000-template.md)・検査スクリプトは同じ変更で合わせる。goal 23の統合ADRの残りや置き換えの鎖をこの方針で組み直すかは、着地後にplannerが人と決める。
