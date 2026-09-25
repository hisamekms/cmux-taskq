---
id: adr-0006
type: adr
title: repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する
status: accepted
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - persistence
  - cli
  - repository
related:
  - design-persistence
  - design-supervisor-lifecycle
  - design-plugin-integration
  - adr-0003
  - adr-0005
---

# ADR-0006: repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する

## Context

ステップ4までのCLIは`--db PATH`を必須とし、queueの場所は利用者が決めていた。pluginのlauncherは`<Git common dir>/taskq/queue.db`を既定にしてこれを補っていたが、規則がバイナリの外にあり、run dir（`<db>.runs/`）とworktreeがDBの隣に置かれるため、`.git`配下に実行時ファイルが増えていた。`supervise`と`integrate`はさらに`--repo`を要求し、同じrepositoryを2つの引数で二重に指定していた。

ドッグフーディング（ステップ9）では1つのrepositoryに1つのqueueがあれば足り、SVとworker、pluginのskill、専用ターミナルのsupervisorがどのworktreeからでも同じqueueに着くことが重要になる。

## Decision

- queueはrepositoryごとに1つとし、`$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`（`XDG_DATA_HOME`が未設定・空・相対pathなら`$HOME/.local/share`）に置く。`<hash>`はcanonicalizeしたGit common directoryのUTF-8 bytesのSHA-256のhex先頭16文字で、同じdirに`repository`ファイル（common directoryのpath）を置いて人が逆引きできるようにする。
- run dir・worktree・ログはDBと同じdirの`runs/<run-id>/`に置く。`--db PATH`でも`dirname PATH`/`runs/`とし、規則を1つにする。
- CLIは`--db`がなければcwdの`git rev-parse --path-format=absolute --git-common-dir`からqueueを解決する。どのworktree（run worktreeを含む）からでも同じqueueになる。`--db PATH`は使い捨てrepositoryとテスト用の明示overrideとして残す。`supervise --repo`と`integrate --repo`は任意のoverrideになり、既定はcwdのcheckout。
- cwdから解決したqueueは`init`の時点で`queue_repository`にcommon directoryを束縛し、以後の全コマンドがopen直後に一致を検査する。`--db`のqueueは従来どおり最初の`supervise`で束縛し、`supervise`と`integrate`が検査する。
- `locate`サブコマンドが解決結果（`db`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`、`db_exists`）をDBを開かずに返す。pluginのlauncherはDBのpathを計算せず、`CMUX_TASKQ_DB`があるときだけ`--db`を付けてバイナリに渡す。

## Alternatives

- `<Git common dir>/taskq/`に置き続ける: `.git`配下にworktreeとログが積まれ、`git worktree`の管理領域と混ざる。repositoryを削除するとqueueも消える一方、複数repositoryのqueueを一覧できない。
- repositoryのpathをそのままディレクトリ名にする: 長く、区切り文字のescapeが要る。hashと`repository`ファイルの組で同じ情報を保てる。
- 1つのDBに全repositoryのqueueを入れる: queue単位のlease、束縛、schema migrationが複雑になる。cross-repository依存は当面scope外。
- `--db`を廃止する: テストと使い捨てrepositoryでユーザーDIRを汚さずに動かす手段が要る。`XDG_DATA_HOME`の差し替えでも可能だが、明示overrideの方が単純。

## Consequences

- repositoryを移動するとcommon directoryのhashが変わり、新しい空のqueueに解決される。旧queueは`repository`ファイルで特定して`--db`で開くか、手で移す。自動移行は行わない。
- 同じrepositoryを別のpath（symlinkを含む）から使ってもcanonicalizeで同じhashになる。hash衝突やデータdirの複製で別repositoryのqueueに当たった場合は束縛検査が拒否する。
- 実行時ファイルはworktreeの外にあり、`supervise`の「DBはworktree外かcommon dir配下」の制約を常に満たす。
- pluginのskillは`--db`と`--repo`を渡さず、cmux workspaceの`--cwd`をrepositoryにするだけで同じqueueに着く。
