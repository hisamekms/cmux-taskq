---
id: adr-0042
type: adr
title: ADRは丸ごと置き換え、置き換えの日付はsuperseded_on、廃止の日付はdeprecated_onに分けてfrontmatterと本文冒頭の注記に残す
status: superseded
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
superseded_by: adr-t598-1
superseded_on: 2026-09-26
supersedes:
  - adr-0035
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - docs-frontmatter
  - adr-index
  - adr-0024
  - adr-0028
  - adr-0035
---

# ADR-0042: ADRは丸ごと置き換え、置き換えの日付はsuperseded_on、廃止の日付はdeprecated_onに分けてfrontmatterと本文冒頭の注記に残す

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)を読む。

## Context

[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)（2026-09-24にaccepted）は、ADRを丸ごと置き換え、採用日と置き換え・廃止日をfrontmatterに、無効であることを本文冒頭の注記に残すと決めた。その背景は次のとおりで、このADRもそのまま引き継ぐ。

2026-09-24時点のADR 0001〜0034のstatusは`accepted`か`proposed`だけで、[frontmatter](../frontmatter.md)が定める`superseded`と`superseded_by`は一度も使われていなかった。決定の変更は、後のADRの本文の一文で前のADRの決定を番号で上書きする形（例: [ADR-0028](0028-workspace-titles-are-repo-and-role.md)の「この決定はADR-0018の決定1とADR-0021の決定1・2を上書きする」）か、用語集で読み替える形（例: [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)の決定1）で表されてきた。どちらも上書きされた側のADRには何も残らず、古いADRを単独で開いた読み手（人もAIのsessionも）はその決定が今も有効だと読み、退役した役割（maintainer）や旧い名前を前提に判断した。ADRには、いつ採用され、いつ置き換え・廃止されたかも無く、git logにしか残っていなかった。ユーザーは2026-09-24に、決定番号単位の部分的な上書き（`amended_by`）を危険として退け、置き換えは丸ごとにして古いADRに後継のIDを記録し、採用日と置き換え・廃止日をfrontmatterに残し、後継の無い廃止のためにstatusに`deprecated`を足すと決めた。

ADR-0035は、`superseded`にした日と`deprecated`にした日を同じ`superseded_on`に持たせていた。ADR-0035のreview（task 208）のfollow_upを受けて、ユーザーは2026-09-24に、`deprecated`の廃止日は`superseded_on`ではなく専用の`deprecated_on`に持たせると決めた。`superseded_on`という名前は「置き換えられた日」を指し、後継の無い廃止に使うと欄の名前と意味がずれる。また`superseded_on`は後継の`accepted_on`と一致するという検査の手がかりを持つが、`deprecated`にはその対応が無い。

この変更はADR-0035の決定3・5・6（と、決定3に合わせて決定8の日付の列挙）を変える。ADR-0035の決定6（本文の決定を変えるには新しいADRで丸ごと置き換える）により、ADR-0035自身をこのADRで丸ごと置き換える。ADR-0035の規則をADR-0035自身に当てはめる最初の置き換えで、手順の見本にもなる。変えるのは次の点だけで、ほかの決定はADR-0035のまま書き直して引き継ぐ。

- 決定3: `superseded_on`を持つのは`superseded`だけにし、`deprecated`は`deprecated_on`（`deprecated`にした日）を持つ。
- 決定5: 廃止の注記の日付は`deprecated_on`と同じにする。
- 決定6: 後から変えてよい欄に`deprecated_on`を足す。`supersedes`は後から変えてよい欄に入らないことを明記する。
- 決定8: git logで確かめる日付に`deprecated_on`を足す（決定3に合わせるだけで、棚卸しの範囲は変えない）。

## Decision

**原則。** `accepted`のADRは本文の決定がすべて現在有効である。置き換え・廃止されたADRは、開いた時点でstatus・後継ID・日付と本文冒頭の注記から無効と分かり、置き換えられたADRは後継を1本辿れば生きている決定がすべて読める（廃止されたADRには後継が無く、注記が理由を示す）。以下の8点を決める。

1. **statusは5つにする。**
   - `proposed`: 検討中。決定はまだ有効ではない。
   - `accepted`: 採用済み。本文の決定がすべて現在有効。
   - `rejected`: 不採用。
   - `superseded`: 後継のADRに丸ごと置き換えられた。
   - `deprecated`: 後継なしで廃止された。
2. **置き換えは丸ごとにする。**
   - 既存のADRの決定を1つでも変えるADRは、そのADRのまだ生きている決定も書き直して引き継ぎ、古いADRを丸ごと`superseded`にする。
   - 1本のADRで複数のADRを置き換えてよい（統合）。
   - 「ADR-XXXXの決定Nを上書きする」だけの部分的なADRは今後書かない。
3. **frontmatterに次の欄を置く。**

   | 欄 | 持つADR | 値 |
   | --- | --- | --- |
   | `accepted_on` | `accepted` / `superseded` / `deprecated` | `proposed`から`accepted`にした日 |
   | `superseded_by` | `superseded` | 後継のADRのID 1つ。後継も置き換えられていれば、読み手は`accepted`に着くまで辿る |
   | `superseded_on` | `superseded` | `superseded`にした日 |
   | `deprecated_on` | `deprecated` | `deprecated`にした日 |
   | `supersedes` | 置き換えた側 | 置き換えたADRのIDのリスト |

   `rejected`は`accepted_on`を持たない。`deprecated`は`superseded_by`と`superseded_on`を持たず、`superseded`は`deprecated_on`を持たない。`updated`は内容の変更日のままで、statusと上の欄だけの変更では変えない。

   ```yaml
   status: superseded
   created: 2026-09-22
   updated: 2026-09-22
   accepted_on: 2026-09-22
   superseded_by: adr-0040
   superseded_on: 2026-09-25
   ```

   ```yaml
   status: deprecated
   created: 2026-09-22
   updated: 2026-09-22
   accepted_on: 2026-09-22
   deprecated_on: 2026-09-25
   ```

4. **置き換えは、後継を`accepted`にする変更と同じ変更で行う。**
   - 古いADRの`superseded_on`は後継の`accepted_on`と同じ日にする。
   - `proposed`の後継は何も置き換えない。後継を`proposed`で書くときは`supersedes`に置き換える予定のIDを書いてよいが、古いADRのstatusは後継を`accepted`にするまで変えない。
5. **`superseded` / `deprecated`のADRは、H1の直後に1行の注記を置く。**

   ```markdown
   > **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
   ```

   ```markdown
   > **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
   ```

   置き換え済みの注記の日付は`superseded_on`と、廃止の注記の日付は`deprecated_on`と同じにする。
6. **本文はappend-onlyのままにする。**
   - 後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`と、決定5の注記1行だけで、これらの変更には新しいADRは要らない。
   - `supersedes`は後から変えてよい欄に入らない。置き換える側のADRを書くときに、本文（Contextの置き換えの理由と、引き継ぐ決定）と一緒に書く欄だからである。置き換える対象を後から足す・変えるなら、その本文も変わるので新しいADRで行う。
   - それ以外の本文の変更（決定の追加・変更・削除）は新しいADRで行い、決定2に従って丸ごと置き換える。
7. **[docs/adr/README.md](README.md)を索引にする。**
   - 有効な（`accepted`の）ADRの一覧と、`superseded` / `deprecated`のADRから後継への対応表を置く。
   - ADRのstatusを変える変更は、同じ変更で索引も更新する。
8. **既存のADR 0001〜0034を棚卸しする。**
   - 後のADRに決定を1つでも上書き・読み替えされたADRは、統合ADRで丸ごと置き換える。
   - `accepted_on` / `superseded_on` / `deprecated_on`の日付はgit logで確かめる。
   - `proposed`のまま実装済みの[ADR-0009](0009-goal-groups-tasks.md)は`accepted`にする。

## Alternatives

- **廃止日も`superseded_on`に持たせる（ADR-0035のまま）**: 欄が1つ少ないが、欄の名前が「置き換えられた日」を指すので後継の無い廃止と意味がずれ、`superseded_on`が後継の`accepted_on`と一致するという対応も`deprecated`では成り立たない。ユーザーが専用の欄に分けると決めた。
- **ADR-0035の本文を書き換えて決定3・5・6だけ直す**: ADR-0035の決定6（append-only）に反し、この規則が最初に破る例になる。
- **決定番号単位の`amended_by`**: 古いADRに「決定Nは後のADR-XXXXが変えた」と記録する。読み手は古いADRと後のADRを突き合わせて、どの決定が生きているかを推論しなければならず、番号の参照の誤りも検出しにくい。ADR-0035でユーザーが危険と判断した。
- **古いADRの本文を書き換える**: 読み手は常に現行の決定を読めるが、決定を採ったときの理由と当時の前提が失われる。append-onlyの原則に反する。
- **用語集・索引だけで読み替える（ADR-0024の方式）**: 古いADRを単独で開いた読み手は用語集に辿り着かず、古い決定を有効と読む。
- **廃止日を`updated`で表す**: 内容の変更日と区別できない。statusを変えただけの日と本文を書き足した日が同じ欄に混ざる。

## Consequences

- 読み手は`accepted`のADRだけを読めばよい。`superseded`のADRは注記から後継に辿り着ける。
- 決定の番号はADR-0035と同じにそろえた。ADR-0036・0040・0041・0043・0045などの本文がADR-0035の決定N（append-onlyの決定6、棚卸しの決定8など）を参照している箇所は、本ADRの同じ番号の決定として読める（本文は決定6のとおり書き換えない）。
- 置き換えと廃止の日付が別の欄になり、`superseded_on`は常に後継の`accepted_on`と一致する。`deprecated_on`には対応する後継が無い。
- 決定を変えるADRのコストが上がる。生きている決定を書き直して引き継ぐ必要があり、1本が長くなる。このADRもADR-0035の決定をすべて書き直している。
- 棚卸しで統合ADRを複数書く。候補は、maintainerの退役（0010 / 0016 / 0019 / 0021 / 0022 / 0023 / 0024）、workspaceの名前と識別（0018 / 0021 / 0026 / 0028 / 0031）、検証・review・着地（0008 / 0023 / 0027 / 0029）。範囲は棚卸しで確定する。
- ADR-0024の決定1の用語集での読み替えは、統合ADRが置き換えることで不要になる。[overview](../design/overview.md)の用語集は役割の定義として残る。
- [frontmatter.md](../frontmatter.md)・[docs/adr/README.md](README.md)・[template](0000-template.md)・[docs/README.md](../README.md)・AGENTS.mdは、この変更と同じ変更でこの決定に合わせる。
- frontmatterの整合（statusと欄の組み合わせ、`superseded_by`の参照先、日付の一致）の機械的な検査はこの決定の範囲外。
