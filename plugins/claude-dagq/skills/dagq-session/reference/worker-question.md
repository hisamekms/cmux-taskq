# dagq-session: a worker's question (`worker_question`)

A worker that needs a decision runs `dagq ask --run <run-id> --kind worker_question --question '...'`, reports briefly and stops. The ask shows in `status` under `asks` (`kind` `worker_question`, with its `run_id`) and as attention `answer ask <id>`. Do not type into the worker's terminal yourself: the supervisor delivers the answer.

- The question is about the run's own worktree (which of two ways to implement, how to read the task, a test to add): answer it yourself with `"$DAGQ" answer <id> --text "<answer>"`.
- Anything else (acceptance, scope, anything the user must decide): register the same question for the user, `"$DAGQ" ask --kind decide --run <run-id> --question "<the worker's question, self-contained>"` (add `--option` when the choices are clear). When its answer arrives as `ask_answered` from your `watch`, forward it unchanged with `"$DAGQ" answer <worker ask id> --text "<the user's answer>"`, then `"$DAGQ" ask close <decide ask id>`.

Once the worker ask is answered and the worker went idle after asking (its idle marker is newer than the ask), the supervisor types `answer to ask <id>: <answer>` and Enter into the worker's terminal once, closes the ask and records `ask_delivered`. Until then `status` shows `delivering the answer of ask <id> (runtime)`; nothing to do. Answering it does not wake your `watch`.

Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run no longer `running`): the supervisor could not type the answer (it tries once), or the session is gone. When the run is still `running`, read the worker's screen, send the text `answer to ask <id>: <answer>` followed by `enter` as in `reference/cmux.md`, then `"$DAGQ" ask close <id>`. When the run is at rest, the answer has nobody to reach: tell the user and close the ask.
