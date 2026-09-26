---
name: release
description: dagq 自身のリリース手順。version を上げる、tag v* を切る、GitHub Release と crates.io に出す、release.yml の成功と crates.io の自動 publish（Trusted Publishing）を確認するときに使う。利用者向けではなく、この repository を開発する session 用。
---

# dagq をリリースする

根拠は [README の Release 節](../../../README.md#release) と [ADR-0030](../../../docs/adr/0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md)。tag `vX.Y.Z` の push で `.github/workflows/release.yml` が、tag と `Cargo.toml` の version の一致を検査し、`aarch64-apple-darwin` のバイナリを build して GitHub Release に添付し、同じ version を crates.io に publish する（crates.io に既にあれば skip）。

main の version はリリースの直後に次の開発版 `X.Y.Z-dev` へ上げてあり（今は `0.4.0-dev`）、main のビルドは `dagq --version` で build 識別子 `X.Y.Z-dev+<commit>`（worktree が dirty なら `.dirty` が付く）を名乗る。リリースは `-dev` を外した `X.Y.Z` で、build 識別子も `X.Y.Z` だけになる（[ADR-0073](../../../docs/adr/0073-kind-additions-are-compatible.md) 決定 1・2。ADR-0045 を置き換えた。`build.rs` が埋め込む）。

以下 `X.Y.Z` は新しい version。1・3・4・5 のコマンドは repository の main checkout で打つ（読むだけで、main の作業ファイルは変えない）。2 と 6 の変更は task の worker が worktree で行う。tag の作成と push はユーザーの確認を取ってから行う。

## 1. 前提の確認

```sh
git fetch origin
git switch main && git pull --ff-only
git status --porcelain            # 何も出ないこと（clean）
git rev-parse HEAD origin/main    # 2 行が同じこと
gh run list --branch main --workflow ci.yml --limit 5   # 最新の main の run が success
cargo publish --dry-run --locked  # package の作成と build が通ること
```

- `gh run list` の先頭が main の HEAD の commit で `completed success` でなければリリースしない（`gh run view <run-id> --log-failed` で原因を見る）
- `cargo publish --dry-run --locked` は PATH の `dagq`（`~/.local/bin/dagq`）とは無関係に、package の中身（`build.rs`・`src/`・`migrations/`・`Cargo.toml`・`Cargo.lock`・`README.md`・`LICENSE`）だけで build できるかを見る。`cargo package --list --locked` で中身を確かめられる

## 2. `-dev` を外して version を確定する

`Cargo.toml` の `[package]` の `version` と `plugins/claude-dagq/.claude-plugin/plugin.json` の `"version"` を同じ `X.Y.Z` にし（main の `X.Y.Z-dev` から `-dev` を外す。上げる桁を変えるなら、ここで `-dev` の数字と違う `X.Y.Z` にしてよい）、`Cargo.lock` を更新する。tag は `-dev` の無い commit に打つ（`release.yml` は tag と `Cargo.toml` の version の一致を検査するので、`-dev` が残っていれば落ちる）。

worker が task の worktree で行う:

```sh
# version を書き換えた後
cargo update --workspace   # Cargo.lock の dagq の version を更新
# 確認
grep -n '^version' Cargo.toml
grep -n '"version"' plugins/claude-dagq/.claude-plugin/plugin.json
git diff --stat                                                  # Cargo.toml / Cargo.lock / plugin.json だけ
```

- main に直接 commit しない（AGENTS.md）。この変更も dagq の task として登録し、`integrate` で main に着地させる。例（planner の session から、固定バイナリ `~/.local/bin/dagq` で）:

  ```sh
  dagq add 'release: version を X.Y.Z に上げる' \
    --description 'Cargo.toml と plugins/claude-dagq/.claude-plugin/plugin.json の version を X.Y.Z にし、Cargo.lock を更新する' \
    --acceptance 'Cargo.toml・plugin.json・Cargo.lock の dagq の version が X.Y.Z で、cargo publish --dry-run --locked が通る' \
    --paths Cargo.toml --paths Cargo.lock --paths 'plugins/claude-dagq/.claude-plugin/plugin.json' \
    --verify 'cargo publish --dry-run --locked' --verify 'cargo test --locked --test plugin'
  ```

  登録の書式は dagq skill に従う。着地後に 1 の前提の確認をやり直す
- semver の目安（1.0 未満なので minor が互換を切る単位）:
  - migration で queue の schema が変わる、CLI のコマンド・flag・出力の非互換、receipt や `dagq.toml` の書式の非互換 → minor 以上（`0.3.x` → `0.4.0`）
  - 互換を保つ修正・機能追加だけ → patch（`0.3.0` → `0.3.1`）
  - 迷ったら `git log vPREV..origin/main -- migrations/` で migration の有無を見る。`vPREV` は前のリリースの tag（`git tag --list 'v*'`）。0.3.0 は手で publish したので `v0.3.0` の tag は無い。0.3.0 からの差分を見るときは `v0.2.0` か、version を 0.3.0 にした commit（`git log -S 'version = "0.3.0"' --oneline -- Cargo.toml`）を起点にする

## 3. tag を切る（ユーザーの確認を取ってから）

「`vX.Y.Z` を main の `<HEAD の短い SHA>` に打って push してよいか」をユーザーに聞き、了承を得てから:

```sh
git tag vX.Y.Z origin/main
git push origin vX.Y.Z
```

一度 publish した version は crates.io から消せない（yank だけ）。tag を打ち直すことになっても crates.io の version は上書きできないので、確認は push の前に行う。

## 4. 確認

```sh
gh run list --workflow release.yml --limit 3     # tag vX.Y.Z の run を探す
gh run watch <run-id> --exit-status              # 完了まで待つ。失敗なら非 0
gh run view <run-id>                             # すべての step が成功していること
gh release view vX.Y.Z --json assets --jq '.assets[].name'
# dagq-vX.Y.Z-aarch64-apple-darwin.tar.gz と SHA256SUMS の 2 つが出ること
curl -sS -A 'dagq-release (https://github.com/hisamekms/dagq)' \
  https://crates.io/api/v1/crates/dagq | jq -r '.crate.max_version'
# X.Y.Z が出ること
```

crates.io の API は User-Agent の無い request を拒むので `-A` を付ける。

## 5. 初回の自動 publish の確認（Trusted Publishing に切り替えて最初のリリースだけ）

0.3.0 は手で publish し、Trusted Publisher（owner `hisamekms`、repository `dagq`、workflow `release.yml`、environment なし）は登録済み。tag による自動 publish はまだ一度も走っていないので、最初のリリースでだけ次を確かめる。

```sh
gh run view <run-id> --log | grep -E 'Authenticate to crates.io|Publish to crates.io|already on crates.io'
```

1. `Authenticate to crates.io` の step（`rust-lang/crates-io-auth-action@v1`）が成功している（token は log で mask されるので、見えるのは step の成功だけ。失敗なら OIDC の交換のエラーが出る）
2. `Publish to crates.io` の step で `cargo publish --locked` が成功し、`Uploaded dagq vX.Y.Z` / `Published dagq vX.Y.Z` が出ている。4 の `max_version` も `X.Y.Z`
3. workflow を rerun すると publish が skip される:

   ```sh
   gh run rerun <run-id>
   gh run watch <run-id> --exit-status
   gh run view <run-id> --log | grep 'already on crates.io'
   # "dagq X.Y.Z is already on crates.io; skipping publish" が出て、
   # Authenticate / Publish の step が skipped、run は success
   ```

確認できたら、dagq の planner にその旨（run の URL、`max_version`、rerun で skip されたこと）を伝え、goal 18 を閉じてもらう（goal を閉じるのは planner）。2 回目以降のリリースでは 5 は不要。

### 失敗したときの見どころ

- **tag と version の不一致**: `Check that the tag matches the crate version` が `tag vX.Y.Z declares version ... but Cargo.toml has ...` で落ちる。build も publish もされない。tag を消して（`git push origin :refs/tags/vX.Y.Z` と `git tag -d vX.Y.Z`、ユーザーの確認を取ってから）version を上げた commit に打ち直す
- **Trusted Publisher の不一致**: `Authenticate to crates.io` が失敗する。crates.io の `dagq` の Settings → Trusted Publishing の owner `hisamekms` / repository `dagq` / workflow filename `release.yml` / environment（空）が、実際の repository と workflow のファイル名に一致しているかを見る。登録を直したら `gh run rerun <run-id> --failed`（GitHub Release は `--clobber` で上書きされる）
- **id-token 権限**: OIDC token が取れないエラー（`ACTIONS_ID_TOKEN_REQUEST_URL` が無い等）は、`release.yml` の `permissions:` に `id-token: write` が無いか、fork からの run。workflow の変更は dagq の task にする
- **crates.io の API の失敗**: `crates.io answered HTTP <status>` で止まったら、時間を置いて rerun する
- GitHub Release が出た後に crates.io だけ失敗しても、原因を直して rerun すれば Release は上書き、crates.io は publish される（ADR-0030 の Consequences）

## 6. 次の開発版へ上げる

tag の run が成功したら（4 の確認の後）、main の version を次の開発版 `X.Y.Z-dev` に上げる task を登録する。ここでの `X.Y.Z` は次のリリースの見込みで、普段は minor を 1 つ上げる（`0.4.0` の後は `0.5.0-dev`）。互換を保つ修正だけのリリースが続くと分かっていれば patch でもよく、次のリリースの 2 で改めて決め直せる。2 と同じく `Cargo.toml`・`plugin.json`・`Cargo.lock` の 3 つを変える:

```sh
dagq add 'release: version を次の開発版 X.Y.Z-dev に上げる' \
  --description 'vPREV のリリース後、Cargo.toml と plugins/claude-dagq/.claude-plugin/plugin.json の version を X.Y.Z-dev にし、Cargo.lock を更新する' \
  --acceptance 'Cargo.toml・plugin.json・Cargo.lock の dagq の version が X.Y.Z-dev で、main のビルドの dagq --version が X.Y.Z-dev+<commit> を出す' \
  --paths Cargo.toml --paths Cargo.lock --paths 'plugins/claude-dagq/.claude-plugin/plugin.json' \
  --verify 'cargo publish --dry-run --locked' --verify 'cargo test --locked --test plugin'
```

これを忘れると、main のビルドがリリースと同じ `X.Y.Z` を名乗り、build 識別子に commit が入らないので、`up`・`dagq install`・自動更新がリリースのバイナリと開発中のビルドを見分けられない。

## 7. 固定バイナリの更新

この skill の中では `~/.local/bin/dagq` を `cp` で置き換えない（macOS では走行中のプロセスが kill されうるうえ、前のバイナリが残らない）。更新は [ADR-0073](../../../docs/adr/0073-kind-additions-are-compatible.md) の `dagq install` と自動更新で行い、手順は AGENTS.md の「作業中」と「起動と停止」と、plugin の `dagq-recover` skill の `reference/update.md` に従う。

- 本番の supervisor が `up --auto-update` で動いていれば、2 の `-dev` を外す commit と 6 の次の開発版へ上げる commit はどちらも `Cargo.toml` と `Cargo.lock` を変えるので、それぞれの着地で supervisor が main をビルドし、固定バイナリと自分を引き継ぎで入れ替える（走っている run は止まらない）。着地の後に inbox の `report the update` か `dagq status` の `auto_update` と `supervisors[].binary_version` で、6 の着地の後に `X.Y.Z-dev+<commit>` になったことを確かめる
- 自動更新が無効なら、6 の着地の後に inbox か planner の session から、ユーザーに報告してから `dagq install` を打つ（main checkout を build し、確認・差し替え・supervisor の引き継ぎまで行う）。リリースの間に非互換（`-- dagq-schema: breaking`）の migration が入っていて `install` が止まったら、開いた ask を人に見せ、ユーザーの了承を得てから `dagq install --allow-breaking`（drain して DB を退避し、migrate して起動し直す）を打つ
- 自動更新が有効でも、非互換の migration を含むビルドは入れ替えられず、inbox に `approve_update` の ask が出る。人が `install` と答えたら、問いに書かれた `install --allow-breaking` のコマンドを打ち、それが起動し直した supervisor には `--auto-update` が付かないので、続けて AGENTS.md の cold start の `up`（`--auto-update` 付き）を打ち直す
