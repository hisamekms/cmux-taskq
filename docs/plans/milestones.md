---
id: plan-milestones
type: plan
title: Project milestones
status: active
created: 2026-09-22
updated: 2026-09-22
milestone: roadmap
owners:
  - hisamekms
---

# Project milestones

## M0: Documentation baseline

ADR、現在の設計、MVP計画を確定する。設計変更は影響範囲に応じてADRへ追記する。

## M1: Claude Code first dogfooding

Claude Codeのみで、登録 → SQLiteによるclaim → cmux/worktreeで実行 → receipt検証 → workspace終了 → 手動統合 → 依存解放を通す。単一repository・同時実行1件から始め、最小のlease/heartbeat、障害時の保持、明示復旧、ローカルClaude Code pluginを含める。

完了条件は、cmux-taskq自身で独立task、依存task、失敗からの復旧をDBの手修正なしで実行できること。具体的な順序は[Active plan](current.md)に記載する。

達成（2026-09-22）。[current.md](current.md) ステップ1〜9。

## M2: Provider expansion and runtime hardening

ドッグフーディングで見つかった問題を修正し、Codex provider、provider選択、Claude起動不能時のfallbackを追加する。継続運用と復旧を安定させる。

## M3: Distribution

バイナリのリリースとClaude Code/Codexプラグインの配布を整備し、既存のPythonキューからの移行手順を公開する。
