---
id: adr-0001
type: adr
title: Rustでruntimeを実装する
status: accepted
created: 2026-09-21
updated: 2026-09-21
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - rust
related:
  - design-overview
  - plan-rust-runtime-mvp
---

# ADR-0001: Rustでruntimeを実装する

## Context

cmux-taskqはSQLite、プロセス監視、cmux操作、Claude/Codexの起動を行う。Pythonのスクリプト集合でも試作できるが、プロセスライフサイクルとproviderの抽象化が増えると実行環境の差異と状態管理の複雑さが大きくなる。配布は利用者が個別にPython環境を用意しなくて済むバイナリ形式を目指す。

## Decision

runtimeとsupervisorはRustで実装し、単一の`cmux-taskq`バイナリとして配布する。ドメイン、application、infrastructureをcrateまたはmoduleとして分離する。

## Alternatives

- Pythonスクリプト集合: 試作は速いが、配布と長期的なプロセス管理に追加の前提が必要。
- TypeScript/Node.js: アプリとの共有はあるが、runtime利用者へのNode環境依存が残る。

## Consequences

Rustのビルドとクロスプラットフォーム配布が必要になる。一方、SQLite・プロセス・ファイル・CLIを自己完結したバイナリで提供できる。
