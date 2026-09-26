---
id: design-supervisor-lifecycle-build-identifier
type: design
title: "Build identifier"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0045
  - design-plugin-integration
---

# Build identifier

バイナリは自分をbuild識別子で名乗る（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定1〜3）。`build.rs`がビルド時に`DAGQ_BUILD_ID`を埋め込み、`dagq::VERSION`、`dagq --version`、`register_supervisor`が書く`supervisors.binary_version`（`status` / `doctor`の`binary_version`）、`up`の結果の`version` / `previous_version`、supervise logの先頭行と`rebind`のlogの`version`はすべてこれを出す。規則は`src/build_id.rs`（`build.rs`が`#[path]`で同じファイルを読むので、libのunit testが規則を検査する）:

- `Cargo.toml`のversionにpre-releaseが無い（リリース`X.Y.Z`）ときは`X.Y.Z`だけ。gitは呼ばない（crates.ioのsourceからのリリースのビルドはgitに依らない）。
- pre-releaseがある（mainの`X.Y.Z-dev`）ときは`X.Y.Z-dev+<commit>`。`<commit>`は`git rev-parse --verify HEAD`の短縮しないSHAで、`git status --porcelain`（untrackedを含む）が空でなければ`.dirty`を付ける。gitは`--no-optional-locks`で呼ぶ。
- gitが無い、HEADが読めない、またはgitのtoplevelがpackageのroot（`CARGO_MANIFEST_DIR`）でない（crates.ioのsourceが別のrepositoryの中に展開された、など）ときは`X.Y.Z-dev+unknown`。
- `build.rs`の再実行のきっかけは`build.rs`、`src/build_id.rs`、`src/`・`migrations/`・`Cargo.toml`・`Cargo.lock`（pre-releaseでは、gitの問い合わせより前に登録するので`unknown`に落ちたビルドもsourceの変更で問い直す）と、gitのworktreeの`HEAD`・`index`・`packed-refs`・今のbranchのref（`refs`全体は全worktreeで共有なので見ない。他のworktreeのcommitでこのworktreeのビルドを作り直さないため）。commitとcheckoutとstageで識別子は追従するが、これら以外（docsなど）だけを変えたworktreeは、次に何かが`build.rs`を再実行させるまで`.dirty`にならない。
- `cargo package` / `cargo publish --dry-run`の検証は`target/package/`に展開した写しを元のcheckoutの`target/`でビルドし、cargoはこれを同じunitとみなすので、写しの`unknown`の出力がcheckoutの次のビルドに流用されうる。`unknown`のときは写しの`<CARGO_MANIFEST_DIR>/.git`（存在しない）を見張りに加え、存在しないpathは常に再実行させるので、checkoutの次のビルドはcommitを問い直す。

`up`は識別子の文字列の全体で比べるので、package versionが同じでもcommitかdirtyが違うsupervisorは入れ替えの対象になる。pluginのlauncherがバイナリとの互換を見るのは従来どおりmajor.minorだけ（[plugin-integration](../plugin-integration.md)）。mainのversionの上げ方（tagで`-dev`を外し、リリース後に次の開発版へ上げる）はrelease skill（`.claude/skills/release`）が持つ。
