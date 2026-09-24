---
id: adr-0035
type: adr
title: ADRは丸ごと置き換え、置き換え・廃止の日付とstatusをfrontmatterと本文冒頭の注記に残す
status: proposed
created: 2026-09-24
updated: 2026-09-24
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
---

# ADR-0035: ADRは丸ごと置き換え、置き換え・廃止の日付とstatusをfrontmatterと本文冒頭の注記に残す

## Context

2026-09-24時点のADR 0001〜0034のstatusは`accepted`か`proposed`だけで、[frontmatter](../frontmatter.md)が定める`superseded`と`superseded_by`は一度も使われていない。決定の変更は次の2つの形で表されてきた。

- 後のADRの本文の一文で、前のADRの決定を番号で上書きする。例: [ADR-0028](0028-workspace-titles-are-repo-and-role.md)の「この決定はADR-0018の決定1とADR-0021の決定1・2を上書きする。ADR-0018とADR-0021の本文は書き換えない」。
- 用語集で読み替える。例: [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)の決定1の「ADR-0010以降のADRに残るmaintainerの記述は書き換えず、overviewの用語集で読み替える」。

どちらも、上書きされた側のADRには何も残らない。古いADRを単独で開いた読み手（人もAIのsessionも）は、その決定が今も有効だと読み、退役した役割（maintainer）や旧い名前を前提に判断する。どの決定が生きているかを知るには、後のADRを全部読んで番号の参照を突き合わせるしかない。またADRには、いつ採用され、いつ置き換え・廃止されたかが無く、git logにしか残っていない。`updated`は内容の変更日で、statusの変化の日とは区別できない。

2026-09-24のユーザーとの対話で、当初は決定番号単位の`amended_by`（古いADRに「決定Nは後のADR-XXXXが変えた」と記録する）を提案した。ユーザーは「危険。廃止したADRに次のバージョンのIDを記録するのではダメ？」と指摘し、部分的な上書きを認めず、置き換えは丸ごとにして古いADRに後継のIDを記録すると決めた。続けて「いつ作られ、いつ廃止されたか分かるか」と問い、採用日と置き換え・廃止日をfrontmatterに残すと決めた。後継の無い廃止のためにstatusに`deprecated`を足す。

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
   | `superseded_on` | `superseded` / `deprecated` | `superseded` / `deprecated`にした日 |
   | `supersedes` | 置き換えた側 | 置き換えたADRのIDのリスト |

   `rejected`は`accepted_on`を持たない。`updated`は内容の変更日のままで、statusと上の欄だけの変更では変えない。

   ```yaml
   status: superseded
   created: 2026-09-22
   updated: 2026-09-22
   accepted_on: 2026-09-22
   superseded_by: adr-0040
   superseded_on: 2026-09-25
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

   日付は`superseded_on`と同じにする。
6. **本文はappend-onlyのままにする。** 後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`と、決定5の注記1行だけで、これらの変更には新しいADRは要らない。それ以外の本文の変更（決定の追加・変更・削除）は新しいADRで行い、決定2に従って丸ごと置き換える。
7. **[docs/adr/README.md](README.md)を索引にする。**
   - 有効な（`accepted`の）ADRの一覧と、`superseded` / `deprecated`のADRから後継への対応表を置く。
   - ADRのstatusを変える変更は、同じ変更で索引も更新する。
8. **既存のADR 0001〜0034を棚卸しする。**
   - 後のADRに決定を1つでも上書き・読み替えされたADRは、統合ADRで丸ごと置き換える。
   - `accepted_on` / `superseded_on`の日付はgit logで確かめる。
   - `proposed`のまま実装済みの[ADR-0009](0009-goal-groups-tasks.md)は`accepted`にする。

## Alternatives

- **決定番号単位の`amended_by`**: 古いADRに「決定Nは後のADR-XXXXが変えた」と記録する。読み手は古いADRと後のADRを突き合わせて、どの決定が生きているかを推論しなければならない。番号の参照が誤っていても（番号のずれ、決定の一部だけの変更）検出しにくい。ユーザーが危険と判断した。
- **古いADRの本文を書き換える**: 読み手は常に現行の決定を読めるが、決定を採ったときの理由と当時の前提が失われる。append-onlyの原則に反する。
- **用語集・索引だけで読み替える（ADR-0024の方式）**: 古いADRを単独で開いた読み手は用語集に辿り着かず、古い決定を有効と読む。
- **廃止日を`updated`で表す**: 内容の変更日と区別できない。statusを変えただけの日と本文を書き足した日が同じ欄に混ざる。

## Consequences

- 読み手は`accepted`のADRだけを読めばよい。`superseded`のADRは注記から後継に辿り着ける。
- 決定を変えるADRのコストが上がる。生きている決定を書き直して引き継ぐ必要があり、1本が長くなる。
- 棚卸しで統合ADRを複数書く。候補は、maintainerの退役（0010 / 0016 / 0019 / 0021 / 0022 / 0023 / 0024）、workspaceの名前と識別（0018 / 0021 / 0026 / 0028 / 0031）、検証・review・着地（0008 / 0023 / 0027 / 0029）。範囲は棚卸しで確定する。
- ADR-0024の決定1の用語集での読み替えは、統合ADRが置き換えることで不要になる。[overview](../design/overview.md)の用語集は役割の定義として残る。
- [frontmatter.md](../frontmatter.md)・[docs/adr/README.md](README.md)・[template](0000-template.md)・[docs/README.md](../README.md)・AGENTS.mdは、この決定に合わせて後続のtaskで更新する。
- frontmatterの整合（statusと欄の組み合わせ、`superseded_by`の参照先、日付の一致）の機械的な検査はこの決定の範囲外。
