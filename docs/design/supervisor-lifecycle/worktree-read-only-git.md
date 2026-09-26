---
id: design-supervisor-lifecycle-worktree-read-only-git
type: design
title: "worktreeへの読み取り専用のgit"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# worktreeへの読み取り専用のgit

supervisorがsessionの作業中のworktreeに打つ読み取り専用のgit（`GitRepository::status`・`head`・`current_branch`・`rebase_in_progress`・`conflicted_files`。`SessionWatch` / `ResumeWatch` / `ReviseWatch`のpoll、validating、`integrate`が使う）は`GIT_OPTIONAL_LOCKS=0`で実行する（task 235）。付けないと`git status`はstatの古いentryをrefreshした索引を`index.lock`を取って書き戻すので、同時にsessionが打つ`git add` / `rebase --continue`が`index.lock`で失敗するか、古い索引で上書きされる（task 122で50ms周期にして`AA change.txt`が残った）。`conflicted_files`はporcelainの`git diff`ではなくplumbingの`git diff-files --name-only --diff-filter=U`を使う。`git diff`は`GIT_OPTIONAL_LOCKS=0`でもrefreshした索引を書き戻すため。
