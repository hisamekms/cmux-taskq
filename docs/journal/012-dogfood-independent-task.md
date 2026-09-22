---
id: journal-012
type: journal
title: Dogfooding: one independent task
status: draft
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: null
depends_on_journal: [10]
related:
  - plan-rust-runtime-mvp
---

# 012: Dogfooding: one independent task

## Goal

[plans/current.md](../plans/current.md) ステップ9。ここからドッグフーディングに移行する。cmux-taskq自身のドキュメント改善をタスクとして登録し、固定したビルド済みバイナリで実行して成果をmainへ取り込む。

- runtimeバイナリはrepo外にコピーして使い、作業成果で置き換えない。
- キューDBはステップ5の配置（ユーザーDIR配下、cwdから解決）を使う。
- SVは常駐のClaude Code sessionで、`read-screen`で完了を確認し、差分をレビューして`integrate`を呼ぶ。
- openなジャーナル（013、014、019）を`cmux-taskq add`へ登録し、IDを`queue_task`に書き戻す。

完了条件: 登録から実行、receipt検証、workspace終了、差分と証跡のレビュー、`integrate`によるrebase・再検証・squash着地と`completed`まで、DBの手修正なしで通る。手順をこのジャーナルに記録する。

## Procedure

SVがユーザーの指示を待たずに実行する。開始条件は、[README](README.md)のOpenに`planned` / `open`が残っていないこと（016・017・018・010がdone）。コマンドの引数が下記と違う場合はmainのREADMEを正とし、差分をLogに書く。

1. このジャーナルを`open`にし、開始をユーザーに報告する（待たない）。
2. mainで`cargo build --release --locked`し、`target/release/cmux-taskq`を`~/.local/bin/cmux-taskq`にコピーする。以後この固定バイナリだけを使い、作業成果で置き換えない。バージョンとcommitをLogに書く。
3. repositoryのrootで`cmux-taskq init`を実行し、queueの場所（`~/.local/share/cmux-taskq/<hash>/queue.db`）をLogに書く。
4. taskを登録する。各ジャーナルのGoalを`--description`、完了条件を`--acceptance`、frontmatterの`verify`を`--verify`に写し、返ったIDを`queue_task`に書き戻す。
   - T1: 012の実task。SVがOpenにない小さなドキュメント改善を1つ選ぶ（例: READMEの手順の誤りや欠け）。内容と受け入れ条件をこのジャーナルのLogに書いてから登録する。依存なし。
   - T2: 019（AGENTS.mdの運用置き換え）。`--depends-on T1`。
   - T3・T4・T5: 013のA・B・C。SVがOpenにない小さな改善（テスト追加、doc修正、clippy警告の解消など）を3つ選び、013のLogに書く。AとCは依存なし、Bは`--depends-on A`。013のシナリオの都合で、A・B・CはT2の後に始める（`--depends-on T2`）。
   - T6: 014の実task。SVが小さな改善を1つ選び014のLogに書く。`--depends-on` はBとC。実行中にSVが意図的に失敗または中断させる。
   T1〜T6を`ready`にする。
5. 専用のcmux workspace（名前`TASKQ-SUPERVISOR`）で`cmux-taskq supervise --parallel 4`を起動し、workspace番号をLogに書く。以後、queueの状態は`cmux-taskq list` / `show ID`で見る。
6. runごとのworkspace（`taskq <task> <run>`）を数分おきに`read-screen`し、権限確認・信頼確認には従来どおり応答する。
7. `awaiting_integration`になったrunは`show`でreceiptと検証ログを見て、run branch `taskq/<run-id>`の差分をレビューし、問題なければ`cmux-taskq integrate RUN`を呼ぶ。`needs_session`で止まったら`claude --resume <run-id>`のworkspaceを開き直し、「最新mainへrebaseし、衝突を解消し、検証コマンドを再実行してreceiptを書き直す」と指示する。着地後に`git push origin main`する。
8. T1が`completed`になったら、このジャーナルのResultに手順の実績と詰まりを書いて`done`にし、Openから外す。以後のT2〜T6は013・014のジャーナルにLogを書きながら同じ手順で流す。
9. 失敗・中断したrunは`doctor`で状態を確認し、`recover RUN`のあと`ready ID`で再試行する。DBを手で直さない。詰まったらこのジャーナルのLogに書き、ユーザーに報告して待つ。

## Log

## Result

## Promoted
