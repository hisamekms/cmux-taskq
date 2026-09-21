---
id: plan-claude-lifecycle-spike
type: plan
title: Claude Code lifecycle spike
status: completed
created: 2026-09-22
updated: 2026-09-22
milestone: m1
related:
  - plan-rust-runtime-mvp
---

# Claude Code lifecycle spike

## Result

2026-09-22、使い捨てrepositoryで通常のClaude Codeをcmux内に起動し、変更、unit test、コミット、receipt提出、セッション終了、workspace終了まで確認した。Rust実装前に接続方法を確かめる検証であり、キューやsupervisorの実装完了を意味しない。

| Item | Observed |
| --- | --- |
| cmux | 0.64.25 (106), b685a275c |
| Claude Code | 2.1.278 |
| Python | 3.9.6（検証スクリプトのみ。runtimeはRust） |
| Run ID / Claude session ID | `7bcc7133-55f2-43fd-8cb3-5033141835c8` |
| Workspace UUID | `59E3F403-FBA5-4F49-AC46-86425B987B88`（検証後に終了済み） |
| Result commit | `ddc3454577ee157c6a033e1bd38890355e767e1f`（使い捨てrepository内） |
| Test | `python3 -m unittest -v`: 1 test passed |
| Receipt submitted / session active | `validated: true`, `exit: null`, `safe_to_close: false` |
| After `/exit` | `exit_code: 0`, `safe_to_close: true` |
| Workspace close | `OK workspace:7`。worktreeと成果は保持 |

ローカルの実行記録は `/private/var/folders/3p/g_cty8k11wqf2pg43pwc05g40000gn/T/cmux-taskq-spike-k6s950_n` に保持している。一時領域なので永続保存は保証しない。

## Reproduction

前提はGit、Python 3、cmux、認証済みClaude Code。macOSのsandbox内からはcmux socketへの接続が拒否される場合があり、今回も接続とworkspace操作にはsandbox外での実行許可が必要だった。

1. repository rootで `python3 scripts/claude-spike.py prepare` を実行する。
2. 出力の`root`を記録し、`launch_argv`の各要素を独立した引数として実行する。これはcmux workspaceを作り、wrapper経由でClaudeを通常モードで起動する。
3. 作成されたworkspaceを確認する。信頼確認や権限確認が出る場合は人が判断する。今回の環境では既存のauto mode設定が使われ、入力待ちは発生しなかった。スクリプトはpermission modeを上書きしない。
4. `python3 scripts/claude-spike.py inspect <root>` でreceiptと成果を確認する。Claude自身がテストとコミットを終え、`submit`を呼ぶ。
5. `validated: true`でも`exit: null`ならworkspaceは閉じない。Claudeの応答完了を画面で確認してから、そのworkspaceで`/exit`を実行する。
6. 再度`inspect`し、`safe_to_close: true`を確認してから対象workspaceだけを閉じる。worktreeはレビュー用に残す。

`root`は1回の試行専用で再利用しない。失敗時はworkspaceとworktreeを残し、再試行は`prepare`で新しく作る。workspaceのrefやUUIDは実行ごとに異なる。

## Contract to carry into Rust

- ClaudeにはTTYを継承させ、`--session-id`でrunと紐づける。起動時の位置引数でpromptを渡す。`-p`は使わず、stdoutを通常ファイルへ直接リダイレクトしない。[CLI reference](https://code.claude.com/docs/en/cli-reference)
- 作業完了receiptとプロセス終了は別に保存する。`Stop`は応答終了、`SessionEnd`はセッション終了という別のイベントなので、Stopだけでcleanupしない。[Hooks reference](https://code.claude.com/docs/en/hooks)
- receiptはworktree外のrun管理領域へ一時ファイルからrenameして公開する。run ID、commit、検証結果を含める。
- supervisor側でrun ID、HEAD、baseからの履歴、clean状態、必要なテストを再検証する。このfixtureではテスト変更も拒否した。
- 今回はoperatorが応答完了を確認して`/exit`を送った。Rustではreceipt受領後のidle確認と終了要求の手順を実装する。画面の文言だけに成功判定を依存させない。
- wrapperは子プロセスをwaitし、終了コードを別ファイルへ保存する。子が正常終了した後にworkspaceを閉じる。
- このcmuxでは`new-workspace`に`--json --id-format uuids`を指定しても作成結果は`OK workspace:7`だった。UUIDが必要ならworkspace一覧から解決する。Rust adapterではJSON出力を前提にせず、利用するAPIの応答形式を検証する。

## Additional checks and limits

検証関数について、コミットなし、テスト失敗、dirty worktree、テスト改変を拒否し、正しいコミットを受理することを別の使い捨てrepositoryで確認した。

未検証は権限・信頼確認での待機、Claude異常終了、wrapper/supervisor強制終了、heartbeat、再起動復旧、cleanup失敗。E2Eとsubagent reviewはこの起動検証では対象外と記録した。本番taskで必要な検証を省略できる契約にはしない。

次は[実装計画](current.md)のステップ2（RustとSQLiteの最小キュー）へ進み、終了制御と障害経路はステップ3・4で検証する。
