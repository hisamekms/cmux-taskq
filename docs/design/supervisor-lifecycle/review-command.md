---
id: design-supervisor-lifecycle-review-command
type: design
title: "`review`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0013
---

# `review`

`dagq review ID`は、taskの`awaiting_integration`または`needs_session`のrun（`integrate ID`と同じ選び方）のレビュー資料を`<run_dir>/review.md`に書く（ADR-0016の決定7）。どちらのrunも無ければerrorで、何も書かない。DBは読むだけで、eventも状態も変えない。`head`は`<run_dir>/receipt.json`の`commit`（読めなければerror）。`base`はrunの`base_commit`だが、セッションが`head`を現在の`main`の上にrebase済み（`main`が`base_commit`と異なり`head`の祖先）なら`main`にする。そうしないと`needs_session`から戻るrunのレビューに、その間に着地した他taskの変更が混ざる。Gitは`GitRepository`（runの`repo_path`、無ければworktreeで`inspect`）を通して呼ぶ。ユースケースはapplication層の`src/application/review.rs`の`review(Review, task_id)`で、queueは`TaskStore`、Gitは`Repository`（`log_oneline`・`diff_stat`・`diff_numbers`・diff全文をファイルへ書く`diff_to_file`）、receiptの読み取りと`review.md`の書き込み・rename・一時ファイルの削除は`RunFiles`（フェンスで挟んでコピーする`write_fenced`を含む）越しに行い、runのcheckoutを開く関数と一時ファイル名のpidを受け取る（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1）。`src/compose.rs`の`review(db, task_id)`が`SqliteQueue`・`GitRepository::inspect`・`LocalRunFiles`を渡す入口で、`runtime::review`として再公開し、supervisorのreviewも同じ入口を呼ぶ。

`review.md`の節は順に: 見出し（task id・title、run id・status、base（とrunの`base_commit`）、head、branch、worktree、`integrate-<attempt>-verify-N.log`の場所と最新の試行のlog。検証コマンドは`integrate`のrebase後にだけ走るので、着地前のreviewの時点では無いか、前回までの`integrate`の試行が残したもの）、Task（description、acceptance、verification commands）、Goal（taskにgoalがあるときだけ。acceptanceとconstraints）、Receipt（summary、tests / e2e / subagent_reviewのstatusとevidence_or_reason、follow_ups）、Commits（`git log --oneline <base>..<head>`）、Diffstat（`git diff --stat <base>...<head>`）、最後にDiff（`git diff <base>...<head>`の全文）。diffは`--no-color --no-ext-diff --no-textconv`で取り、コードフェンスは本文のどのbacktick列より長くする。ファイルは`<run_dir>/.review.md.<pid>.tmp`に書いてからrenameする。

文字コードとtimeout: reviewのGit呼び出しは他のGit呼び出し（`adapters::output`、30秒のtimeoutとstdoutのUTF-8要求）を通らず、review用の経路で`REVIEW_TIMEOUT`（300秒）のtimeoutを持つ。Commits・Diffstatと`--numstat`（返り値の`files_changed`/`insertions`/`deletions`）はstdoutをbytesで受けて`String::from_utf8_lossy`で読むので、UTF-8でないコミットメッセージやファイル名は置換文字になるだけで失敗しない。diff全文はメモリに載せず、Gitのstdoutを`<run_dir>/.review.md.<pid>.diff.tmp`へ直接流し、そのファイルをchunkで読んでbacktick列の最長を数えてから、見出し以降のテキストとフェンスで挟んで`.review.md.<pid>.tmp`へコピーする（フェンスの長さはdiffを読み終えるまで決まらないため）。diffはGitが出したbytesのままなので、Latin-1やShift_JISのテキストファイルを含むrunでは`review.md`はUTF-8でないbytesを含む。一時ファイルは成功でも失敗でも消す。

stdoutは`{"run_id","task_id","path","base","head","files_changed","insertions","deletions"}`だけで（数値は`git diff --numstat`の合計。binaryは1ファイル0行）、diff本文は返さない。呼んだsessionは`path`をsubagentに渡して結論だけを受け取り、自分のコンテキストでdiff全文を読まない。
