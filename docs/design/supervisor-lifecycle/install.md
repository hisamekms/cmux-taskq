---
id: design-supervisor-lifecycle-install
type: design
title: "`install`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - design-persistence
---

# `install`

`dagq install [--from PATH] [--to PATH] [--rollback] [--allow-breaking] [--handoff-timeout SECS] [--cmux EXE] [--claude EXE] [--plugin-dir PATH]`は固定バイナリを入れ替えて、このqueueのsupervisorを引き継がせる人の入口（ADR-0045の決定11・12・14）。use caseは`src/application/install.rs`、binaryのビルド・起動・置き換えは`src/infrastructure/binaries.rs`（`Binaries` port）。queueを開く前に処理するので、queueがこのbinaryより古くても新しくても動く。

1. **元**: `--from`がfileならそのbinary、dirならそのcheckout、省略時はcwdのrepositoryのmain checkoutを`cargo build --release --locked`でビルドする（`CARGO_TARGET_DIR`があればそこ、無ければ`<checkout>/target`の`release/dagq`）。`--rollback`は`<to>.previous`（無ければerror）。
2. **確認**（決定12）: 元の`--version`でbuild識別子を読み、使い捨てのqueue（tmpdirの`--db`）で`init`と`list`が通ることを確かめる。通らなければ何も置き換えない。引き継ぎを受けるliveな登録があるのに、元が`supervise --handoff-token`を知らない（`supervise --handoff-token probe --help`が通らない。引き継ぎより前のbinary）ときも、execしたsupervisorが引数で止まるので、何も置き換えずに`down --wait`と`up`を案内するerrorで止まる（`--rollback`で引き継ぎより前のbinaryに戻すときも同じ）。
3. **schema**: queueのDBがあれば元の`migrate --check`を読む。非互換のmigrationがあれば、`--allow-breaking`が無ければ何も置き換えずにerror、あればdrain（下記6）。元がqueueを開けず（floorより古い。非互換のmigrationの後の`--rollback`）、適用するものも無ければ、`backups/`を案内するerror。互換のmigrationがあれば元の`migrate`で適用する。
4. **置き換え**（決定11）: 元を`<to>`と同じdirの一時file（`.<name>.install-<pid>`）にcopyして0755にし、`<to>`をhard linkで`.<name>.previous-<pid>`に残してから一時fileを`<to>`にrenameし、残したものを`<to>.previous`にrenameする。動いているプロセスは前のinodeを使い続けるので、上書きのcpで殺されることはない。`--rollback`は同じ手順で`<to>`と`.previous`が入れ替わる。`<to>`の既定はこのbinary自身（`current_exe`）。
5. **引き継ぎ**: liveな登録（PIDが生きていてheartbeatが30秒以内）のうち引き継ぎを受けられるものに`<to>`をexecさせ（`lifecycle::hand_off`、上限`--handoff-timeout`、既定1800秒）、受けられないものは`not_handed_off`（`up`でdrainする）に載せる。引き継ぎが失敗すれば`<to>.previous`を`<to>`に戻し（`restore`）、その旨を添えたerrorで止まる。結果は`{"outcome":"installed","target","version","previous","previous_version","migrated","supervisors":[{"token","pid","mode","workspace_id","version"}],"not_handed_off":[…]}`。queueが無ければ置き換えだけを行う。
6. **drain**（`--allow-breaking`、決定14）: liveな登録があれば`down --wait`で止め、元の`migrate`（非互換なので`backups/`に複製してから適用）、置き換え、`<to> --db <db> up --parallel <止めた登録のparallel> [--in-cmux（止めた登録がin_cmuxなら）] --cmux … [--claude …] [--plugin-dir …]`で起動し直す。結果は`{"outcome":"installed",…,"migrated","drained","up"}`。

見張り（決定13。execの後に一定時間内に動き出さなければ前のbinaryに戻し、inboxに`update_failed`を立てる）と`up --auto-update`（決定17）はまだ無い。今は`install`自身が引き継ぎの完了を待ち、失敗すればfileを戻してerrorを返す。新しいbinaryが起動直後に死んだときは、supervisorは止まったままなので、inboxの`supervisor_stopped`と`install`のerrorを見て、人が戻ったbinaryで`up`する（leaseはstaleになり、adoptで引き継がれる）。
