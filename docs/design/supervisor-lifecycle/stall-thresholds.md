---
id: design-supervisor-lifecycle-stall-thresholds
type: design
title: "Stall thresholds"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0043
---

# Stall thresholds

`dagq.toml`の`[stall]`（[ADR-0043](../../adr/0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)の決定4）が、止まったworkerのsessionの検知の閾値を秒で持つ。読み込みは`[run.env]`と同じ`src/infrastructure/run_env.rs`（`parse_config`と、fileを読む`load_stall_config`）で、型と既定値は`src/domain/stall.rs`の`StallConfig`。

| 設定名 | 既定値 | 意味 |
| --- | --- | --- |
| `idle_without_receipt_secs` | 1200（20分） | receiptの無いidleが促しまで続く時間と、促しの後にaskまで続く時間。`stats`の`idle_without_receipt`の閾値 |
| `send_confirm_secs` | 60 | 送った文が処理されるのを待つ時間と、Enterの送り直しの後に待つ時間 |
| `background_alert_secs` | 1800（30分） | `stats`の`long_background`の閾値と、supervisorが[復旧job](background-recovery-job.md#backgroundの処理が終わらないときの復旧job)を起動する閾値 |

- 書式は`KEY = 秒`（正の整数。`_`の桁区切りと`#`以降のcommentを許す）。未知のkey、整数でない値、0以下、重複したkeyはエラー。表もkeyも無ければ既定値。
- `stats`は、supervisorが起動時に記録した最新の`stall_config_loaded`（payloadは3つの値）があればその値を使い（出力の`stall_config.source`が`supervisor`）、無ければmain checkoutの`dagq.toml`（`file`）、それも無ければ既定値（`default`）で判定する。main checkoutはqueueが束縛されたGit common directoryが`.git`ならその親で、そうでなければ既定値。supervisorは起動時（登録の直後）にmain checkoutの`dagq.toml`の`[stall]`を読み（fileが無ければ既定値。書式の誤りは起動のエラー）、3つの値と自分の`supervisor` tokenをpayloadにしたtaskの無い`stall_config_loaded`を記録して、その値で[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)を行う（task 288。値を変えたら`down --wait` → `up`）。`send_confirm_secs`を使う送信の確認（ADR-0043の決定2）はgoal 30の後続taskで入る。
