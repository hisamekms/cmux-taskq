---
id: design-supervisor-lifecycle-conflict-thresholds
type: design
title: "Conflict thresholds"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# Conflict thresholds

`dagq.toml`の`[conflicts]`（goal 31）が、`stats`の`conflict_hotspot`のalert（[`stats`](stats.md#stats)の`conflict_hotspots`）の閾値を持つ。読み込みは`[stall]`と同じ`src/infrastructure/run_env.rs`（`parse_config`と、fileを読む`load_conflict_config`）で、型と既定値は`src/domain/stats/conflicts.rs`の`ConflictConfig`。書式は`[stall]`と同じ（正の整数、`_`の桁区切りと`#`以降のcommentを許し、未知のkey・0以下・重複はエラー）。

| 設定名 | 既定値 | 意味 |
| --- | --- | --- |
| `hotspot_conflicts` | 3 | alertにするファイルの、window内の衝突の最少回数 |
| `hotspot_ratio_percent` | 20 | alertにするファイルの、そのファイルを変えた着地の数に対する衝突の割合の最小（%） |

- `stats`はmain checkoutの`dagq.toml`の`[conflicts]`（出力の`conflict_hotspots.config.source`が`file`）、無ければ既定値（`default`）で判定する。supervisorは起動時に同じものを読み（読めなければwarnを出して既定値にし、起動は止めない）、plan reviewのpromptの衝突の多いファイルの`alert`をその値で判定する（値を変えたら`down --wait` → `up`）。
