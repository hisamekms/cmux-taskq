# predictor-replay (task 553 spike)

Replays past integrated runs of this repository's queue and measures how well rules and LLMs predict a
run's weight (output tokens, model time) and rework before it starts. The report is
[docs/plans/spike-predictor-replay.md](../../../docs/plans/spike-predictor-replay.md).

```sh
python3 collect.py             # read-only: ~/.local/bin/dagq stats/events/show + git -> out/dataset.json
python3 predict.py rules       # mechanical features
python3 predict.py haiku       # claude -p, at most 2 calls at once, cached in out/predictions/
python3 predict.py sonnet
python3 predict.py opus-low   # the report also ran opus-high; --only-sample / --inputs b narrow a run
python3 evaluate.py            # -> summary.json, summary.csv
```

Run from inside the repository (the queue is resolved from the cwd). `out/` holds the raw inputs and
predictions and is not committed. `collect.py` asserts for every item that the inputs are cut at the
submit / claim event, that the main commit shown does not contain the run's squash commit, that the
predecessor summaries landed before the claim, and that neither the run id nor the squash commit
appears in the input (`check_no_leak`).
