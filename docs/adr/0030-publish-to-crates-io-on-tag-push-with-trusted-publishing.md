---
id: adr-0030
type: adr
title: crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする
status: accepted
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - distribution
  - release
  - security
related:
  - adr-0005
  - adr-0015
  - design-plugin-integration
---

# ADR-0030: crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする

## Context

配布はGitHub Releaseの`aarch64-apple-darwin`バイナリだけで（[ADR-0005](0005-binary-and-plugin-distribution.md)）、`Cargo.toml`は`publish = false`だった。crates.ioの`dagq`は2026-09-24にAPIで確認した時点で空いているが、[ADR-0015](0015-rename-to-dagq.md)のとおり同名の`justinj/dagq`（`Cargo.toml`の`name`が`dagq`）が先にpublishする懸念がある。名前を確保し、Rustの利用者が`cargo install dagq`で入れられるようにしたい。

publishを人の手に残すと、tagを打つたびにGitHub Releaseとcrates.ioの版がずれうる。自動化するなら認証情報をどこに置くかが問題になる。crates.ioのAPI tokenをrepositoryのsecretに入れる方法は、期限のない書き込み権限を長く置くことになる。

release.ymlは第三者のactionを使わず、`gh`とshellで書く方針をとってきた（task 28）。crates.ioのTrusted Publishingは、GitHub ActionsのOIDC tokenをcrates.ioの短期tokenに交換する手順を要し、公式にはcrates.ioチームの`rust-lang/crates-io-auth-action`で行う。

## Decision

1. **追加の経路**: crates.ioを配布経路に加える。GitHub Releaseのバイナリ配布（archive名、`SHA256SUMS`、pluginのlauncherの案内）は変えない。crates.ioの版はsourceからbuildするもので、対応platformがmacOS Apple Silicon（`aarch64-apple-darwin`）だけという前提も変わらない。`Cargo.toml`から`publish = false`を外し、`repository` / `homepage` / `readme` / `keywords` / `categories`を足す。packageに入れるのは`include`で`src/`、`migrations/`（`include_str!`で埋め込むのでbuildに要る）、`Cargo.toml`、`Cargo.lock`、`README.md`、`LICENSE`だけにし、`docs/`・`plugins/`・`tests/`・`scripts/`・`.github/`・`rust-toolchain.toml`は入れない。
2. **tag pushで自動publish**: `.github/workflows/release.yml`は、既存のtagと`Cargo.toml`のversionの一致検査を通り、GitHub Releaseに添付し終えた後に`cargo publish --locked`する。publishの前に`https://crates.io/api/v1/crates/dagq/<version>`を引き、200（同じversionがある）ならpublishのstepをskipし、404ならpublishし、それ以外のstatusならfailする。Releaseのstepが`gh release upload --clobber`で再実行に耐えるのと同じく、workflowを再実行しても成功する。
3. **Trusted Publishing**: 認証はcrates.ioのTrusted Publishingにする。jobに`permissions: id-token: write`を与え、`rust-lang/crates-io-auth-action@v1`がOIDC tokenを短期のcrates.io tokenに交換し、その出力を`CARGO_REGISTRY_TOKEN`としてpublishのstepにだけ渡す。長期のAPI tokenをsecretに持たない。GitHubのenvironmentは使わない（Trusted Publisherの登録でもenvironmentは空にする）。
4. **第三者actionの例外**: release.ymlで許す第三者のactionは`rust-lang/crates-io-auth-action`だけにする。crates.io自身を運営するRustプロジェクトの公式actionで、Trusted Publishingの交換手順を実装する公式の手段であり、shellで同じことをするとOIDCのaudienceやtokenの失効（job終了時のrevoke）を自前で保つことになるから。他の第三者actionは今までどおり使わない。
5. **導入手順（ユーザーが一度だけ行う）**: Trusted Publisherはcrates.io上に既にあるcrateにしか登録できないので、最初の1回は手でpublishする。
   1. crates.ioにGitHubでloginし、Account Settingsでemail addressを確認済み（verified）にしてから（未確認のaccountはpublishできない）、Account Settings → API Tokensで`publish-new`のscopeを持つtokenを短い期限で作る。
   2. mainのclean checkoutで`cargo login`にそのtokenを渡し、`cargo publish --dry-run --locked`の後に`cargo publish --locked`する（そのときの`Cargo.toml`のversion。名前`dagq`の確保）。済んだらtokenをcrates.ioで失効させ、`cargo logout`する。
   3. crates.ioの`dagq`のSettings → Trusted Publishingで、GitHubのpublisherとしてrepository owner `hisamekms`、repository name `dagq`、workflow filename `release.yml`を登録し、environmentは空にする。
   4. 以後は`Cargo.toml`（とpluginの`plugin.json`）のversionを上げてmainに着地し、`git tag v<version>`をpushする。release.ymlがGitHub Releaseとcrates.ioの両方に同じversionを出す。最初に手でpublishしたversionのtagをpushした場合は、crates.ioのstepがskipされる。

## Alternatives

- **crates.ioのAPI tokenをsecretに置く**: 設定は簡単だが、期限のない書き込み権限がrepositoryのsecretに残り、漏れたときの影響が大きい。Trusted Publishingはjobごとの短期tokenで済む。
- **publishを手作業に残す**: 名前の確保だけならこれで足りるが、tagのたびに手順が増え、GitHub Releaseとcrates.ioの版がずれうる。
- **OIDCの交換をshellで書く**: 第三者actionを避けられるが、crates.ioのtoken交換APIとrevokeを自前で追うことになり、公式actionより壊れやすい。
- **GitHubのenvironment（例: `crates-io`）でpublishを保護する**: 承認者を置く運用が要り、個人repositoryでは得るものが少ない。必要になったら、crates.ioの登録とworkflowの両方にenvironment名を足す。

## Consequences

- `cargo install dagq`でsourceからbuildして入れられる（Rust 1.93以上とCコンパイラが要る）。更新は`cargo install dagq`で上書きしてから`dagq up`。installされる場所は`~/.cargo/bin/dagq`なので、`~/.local/bin/dagq`と並べるとPATHの順で解決が変わる。どちらか1つにする。
- crates.ioのpackageページは`README.md`を表示し、相対linkは`repository`のGitHub URLに書き換えられる。
- 一度publishしたversionは消せない（yankだけ）。tagと`Cargo.toml`の一致検査はbuildの前にあるので、食い違ったtagではpublishされない。
- release.ymlのGitHub Releaseのstepが成功した後にcrates.ioが失敗すると、Releaseだけが出た状態になる。workflowを再実行すればReleaseは上書き、crates.ioはpublishされる。
