---
id: adr-0005
type: adr
title: runtimeをバイナリ、agent integrationをpluginとして配布する
status: accepted
created: 2026-09-21
updated: 2026-09-21
owners:
  - hisamekms
tags:
  - distribution
  - plugins
  - release
related:
  - design-plugin-integration
  - adr-0001
---

# ADR-0005: runtimeをバイナリ、agent integrationをpluginとして配布する

## Context

cmux-taskqは複数のrepositoryで使える開発基盤にする。Claude CodeとCodexにはskillsやhooksの配布機構があるが、runtimeのSQLite・supervisor・process管理はagent pluginとは別の責務である。

## Decision

Rust runtimeを`cmux-taskq`バイナリとして配布し、Claude Code pluginとCodex pluginはskill・hook・adapter設定からそのバイナリを呼び出す。共通repoから各ecosystem向けのmanifestとplugin packageを提供する。

## Alternatives

- pluginだけにruntimeを同梱する: platformごとのバイナリと実行環境の扱いが複雑になる。
- Python runtimeを利用者に要求する: 配布前提と実行環境の再現性に反する。

## Consequences

platformごとのrelease artifact、checksum、インストール手順が必要になる。plugin利用時にruntimeがPATHにあるか、またはpluginの実行可能ファイルが利用できるかを検査する。
