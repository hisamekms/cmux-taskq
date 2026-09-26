---
id: design-supervisor-lifecycle-first-commit
type: design
title: "最初のcommitの観測"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# 最初のcommitの観測

supervisorは`SessionWatch::poll`のたび（1秒ごと）に、`first_commit_observed`がまだのrunのworktreeの`HEAD`を`GitRepository::head`で読み、runの`base_commit`から動いていれば`first_commit_observed`（`commit`=そのHEAD、`base_commit`）を1回だけ記録する。時刻は観測した時点（commitからせいぜい1 tick遅れ）。引き継いだrunは既に記録があれば記録しない。HEADが読めないときはsupervisor logに書いて次のpollで読み直し、runには影響させない。`agent_started`→`first_commit_observed`が`stats`の`startup`で、workerが起動してから作業に入るまで（ダイアログ・読み込み）の長さを見る（goal 11の決定4）。
