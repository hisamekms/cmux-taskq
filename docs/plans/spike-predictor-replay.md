---
id: plan-spike-predictor-replay
type: plan
title: スパイク：過去の run の再現で task の重さと手戻りの予測の担い手を比べる
status: completed
created: 2026-09-26
updated: 2026-09-26
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - worker
---

# スパイク：過去の run の再現で task の重さと手戻りの予測の担い手を比べる

task 553。worker の model と effort を task ごとに選ぶ案（対策 C）の前提として、「その task がどれだけ重いか・手戻りするか」を、選ぶ時点で手元にある情報からどれだけ予測できるかを、予測の担い手（規則・Haiku 4.5・Sonnet 5・Opus 5.5 low / high）ごとに測った。runtime（`src/`）は変えていない。model / effort を変えたときの worker の効果そのものは測っていない（標本の worker は全 turn が `claude-opus-5-5`）。

再現と計測のスクリプトは [scripts/spikes/predictor-replay/](../../scripts/spikes/predictor-replay/)、要約の表は同じ場所の `summary.json` と `summary.csv`。生の入力と予測（`out/`）は commit していない。

## 結論

- **重さ（出力 token・model の時間）の順位は、LLM が本文から十分に予測できる。** plan review のときの入力 (a) で、Spearman は Sonnet 0.79・Opus low 0.87・Opus high 0.89。claim のときの入力 (b) で Sonnet 0.86・Opus low 0.87・Opus high 0.91。後から分かる変更行数の Spearman（0.75）を上回る。
- **軽い model（Haiku 4.5）は、あらかじめ決めた規則は大きく上回るが、同じ標本に当てはめた規則（leave-one-out）とは同程度で、Sonnet に劣る。** Haiku の Spearman は (a) 0.68 / (b) 0.66。手で重みを決めた規則は 0.28 / 0.31、同じ特徴を標本に当てはめた ridge 回帰（leave-one-out）は 0.67 / 0.71。Sonnet (b) との差は −0.20（90% 区間 −0.33〜−0.09）。しかも `claude -p` の Haiku は thinking で 1 回に約 3,900 token を出し、1 回 40 秒かかる。Sonnet は 1 回 6〜9 秒・約 0.02 USD で、Haiku と同じくらいの費用で速く、精度が高い。「軽い model」として選ぶなら Haiku ではなく Sonnet。
- **claim のときの予測は plan review のときより少し良いが、差は小さい。** (b)−(a) の Spearman は Sonnet +0.07（90% 区間 −0.02〜+0.16）、Opus high +0.03（0.00〜+0.06）、Opus low +0.01、規則 +0.03、Haiku −0.02。(b) で増えるのは主に依存元の receipt の要約（65 件中 44 件）で、本文の文面が (a) と (b) で違ったのは 1 件だけ（依存が違ったのは 6 件）。claim で予測し直す価値は「依存元が着地して分かったこと」を足せる分に限られる。
- **手戻り（resume か review の concern）は、どの担い手でもほとんど予測できない。** AUC は規則 0.52〜0.54、Haiku 0.54〜0.62、Sonnet 0.61〜0.65、Opus 0.63〜0.69。31 件の手戻りのうち 19 件は並行する着地との rebase の衝突を含み、3 件は session が外から kill されたことによる resume を含み、どちらも task の本文からは決まらない。衝突と kill だけの手戻りを除いた「task に由来する手戻り」（integrate の検証の失敗の resume か review の concern、77 件中 18 件）でも AUC は 0.51〜0.68 で、重さの予測値をそのまま手戻りの予測に使っても同程度（0.52〜0.68）。手戻りの見込みは model / effort の選択の根拠にしない。
- **Opus は Sonnet より少し良いが、コストに見合うかは微妙。** Opus high (b) は Sonnet (b) より Spearman で +0.055（90% 区間 0.00〜+0.11）、1 回 約 0.043 USD・6.5 秒（Sonnet の約 1.8 倍の費用）。Opus low は Sonnet と差がない（+0.015、−0.05〜+0.09）のに費用は 1.7 倍。どの LLM でも予測 1 回は 0.02〜0.04 USD・10 秒以内で、worker 1 run（出力 token の中央値 約 3.4 万・model の時間の中央値 約 6 分）に比べて小さい（worker の費用そのものは今回測っていない）ので、費用より精度で選んでよい。
- **LLM の token の見込みは順位は良いが、値は 2〜3 倍に偏る。** run ごとの予測 / 実測の比の中央値は Sonnet 2.3 倍、Opus 2.6〜2.7 倍（Haiku は 1.0 倍だが順位が悪い）。prompt に中央値の目安を書いても従わない。`L` を付ける件数も多すぎる（Sonnet・Opus は 65 件中 33〜39 件で、`L` を重いとみなすと上位 3 分の 1 の recall は 1.0、precision は 0.54〜0.64）。選択に使うなら値ではなく、過去の予測の中での百分位（順位）で閾値を決める。

### C を進めるなら

1. **記録する予測**: plan review の job が、task ごとに今回と同じ JSON（size・nature・uncertainty・expected_output_tokens・rework_probability）を 1 回出して記録する。plan review の job の出力に含めるか、別に Sonnet を 1 回（約 0.02 USD、10 秒）呼べば足りる。
2. **選択に使う予測**: claim のときに Sonnet 5（effort は既定）で、本文・依存元の receipt の要約・base の main の情報から予測し直し、`expected_output_tokens` の過去の予測に対する百分位で model / effort を選ぶ。claim の時点で呼べないとき（失敗・時間切れ）は plan review の記録を使う。Haiku は使わない。Opus high は精度が少し良いので、plan review 側で Opus を使っているならその値を使えば足りる。
3. **手戻りの見込みは記録だけにして、選択には使わない**。衝突による resume は task ではなく並行度と着地の順で、kill による resume は host で決まるので、別の対策（衝突の予防など）で扱う。
4. 予測の値そのもの（token の絶対値）は偏るので、選択の閾値は記録が溜まってから百分位で決め直す。今回の標本では、上位 3 分の 1 を Sonnet (b) の順位で当てる precision / recall は 0.82。

## 方法

### 標本

- 母集団: この queue で `integrated` になった run のうち、worker の transcript（`~/.claude/projects/-Users-shinnosukeooyama--local-share-dagq-77067154921b9014-runs-<run_id>-worktree/<run_id>.jsonl`）が残り、main に `Dagq-Run: <run_id>` の trailer の squash commit があるもの 193 件（2026-09-23〜2026-09-26 10:30 UTC に着地）。
- 種類（タイトルの接頭辞から runtime / docs / test・build / plugin / other の 5 つ。application・domain・supervisor・stats などは runtime に寄せた）と、squash commit の変更行数の 3 分位（206 行以下・769 行以下・それ以上）の組を層にし、層の大きさに比例して（空でない層は最低 2 件）seed 553 で無作為に選んだ 65 件が**主標本**。内訳は runtime 37・docs 13・test/build 7・plugin 4・other 4。
- 主標本の手戻りは 6 件しかなかったので、母集団の残りの手戻り 25 件を**補標本**として足し、手戻りの AUC だけ 90 件（手戻り 31 件）で測った（AUC は陽性の割合に依らない。ただし陰性は変更行数で層別した主標本から、陽性は母集団の全件から来ていて、抽出の仕方が揃っていない。限界を参照）。主標本だけの AUC も `summary.json` の `rework_auc_sample_only` にある（陽性 6 件で参考値）。重さの指標は主標本の 65 件だけで測った。

### 入力（予測する側に見せるもの）

`collect.py` が固定バイナリ `~/.local/bin/dagq` の読むだけのコマンド（`stats --full --since 0 --until …`・`events --all --full`・`show --full`）と Git から組み立てる。

- **(a) plan review のとき**: 切断点はその run の claim の前の最後の `task_submitted`。claim の前に `task_submitted` が無いもの（主標本 65 件中 53 件、90 件中 72 件。大半は plan review の導入前に ready になった task で、`ready --bypass-review` もここに入る）は、claim の前の最後の `ready` への遷移。task の本文（title・description・acceptance・context・paths・verification_commands・required_evidence・kind）は現在の値から、切断点より後の `task_edited` の `from` を新しい順に当てて当時の値に戻す。依存も切断点より後の `dependency_added` / `dependency_removed` を戻す。main は切断点の時刻より前の main の first-parent の最新の commit。
- **(b) claim のとき**: 切断点はその run の `run_claimed`。本文と依存は同じく当時の値に戻し、main は run の `base_commit`。加えて、依存元の task のうち切断点より前に `run_integrated` になったものの receipt の要約（その着地の `integration_receipt` の summary）と、この task の以前の run の結果（状態だけ）。
- main から見せるのは、本文が名指すファイル（`src/`・`tests/`・`docs/` などの path と `AGENTS.md`・`Cargo.toml` など）のその commit での行数（ディレクトリならファイル数）、最新の ADR と migration の番号、`src/` と `tests/` の `.rs` の数。ファイルの中身は見せない。

**未来の情報を混ぜていないことの確かめ方**: `collect.py` の `check_no_leak` が全 90 件の (a) と (b) について次を assert し、1 件でも破れれば止まる。(1) 切断点の event id がその run の最初の event（claim）以下。(2) 見せる main の commit がその run の squash commit を含まない（`git merge-base --is-ancestor` が偽）。(3) 依存元の要約はどれも切断点より前に着地したもの。(4) 入力の JSON に run id と squash commit が現れない。(1) と (3) は組み立て方から必ず成り立つ念のための検査で、実質の検査は (2) と (4)。別に subagent の review が、全 180 入力で戻した本文が切断点の前の最後の `task_edited` の `to` と一致すること、(a) の main が切断点の時刻以前の最新の first-parent の commit で (b) の `base_commit` の祖先であることを確かめた。本文の戻しは `task_edited` だけを見るので、別の event で paths を変えた task（`task_paths_changed`。この queue では標本の外の task 225 の 1 件）を使うときは直す必要がある。入力には receipt・transcript・その run の event・後の event を読むコードが無い（依存元の receipt は別の task の、着地済みのもの）。予測する LLM は入力 JSON だけを prompt で受け取り、tool を持たない（`--tools ""`）。

### 予測する側

- **規則**（`predict.py` の `features` と `rule_scores`）: 本文の長さ（log）、acceptance の項目数、paths が docs だけか、依存の数、verify に llvm-cov があるか、e2e の evidence の要求、ADR を書くか、migration の言及、名指すファイルの数と行数（log）、依存元の要約の長さ、以前の run の数。重みは結果を見る前に planner の常識（runtime > test > docs、長い本文・多い acceptance は重い）で手で決めた（`rules`。依存の数・依存元の要約の長さ・以前の run の数は手戻りの score にだけ入れ、重さの score には入れていない）。比較のため、同じ特徴を主標本に当てはめた ridge 回帰の leave-one-out の予測も出した（`rules-loo`。各 run は自分の結果を含まない 64 件で学習した式で予測される）。
- **LLM**: Haiku 4.5（`claude-haiku-4-5-20251001`）・Sonnet 5（`claude-sonnet-5`）・Opus 5.5（`claude-opus-5-5`、`--effort low` と `high`）を `claude -p --model … --strict-mcp-config --tools "" --system-prompt … --output-format json --no-session-persistence` で、CLAUDE.md も project の memory も無い空の cwd と最小の env（HOME・PATH・USER・LANG・TERM）で呼んだ。同じ prompt（dagq の run の流れの説明、出力 token の目安、入力 JSON）で size（S/M/L）・nature（mechanical / implementation / design_judgment / investigation）・uncertainty・expected_output_tokens・rework_probability・reason を JSON で出させた。Haiku と Sonnet の effort は指定していない（既定）。同時の呼び出しは 2 本まで。
- 当初の計画どおり、規則・Haiku・Sonnet を全標本で回し、傾向を見て（Sonnet が Haiku を大きく上回り、手戻りはどれも弱かったので、上の担い手で良くなるかを見るため）Opus low / high を足した。

### 答え（結果）

- 出力 token: transcript の main thread（`isSidechain` を除く）の assistant の `message.usage.output_tokens` を message id ごとに 1 回数えた合計（1 つの message は content block ごとに複数行に書かれ、各行に同じ usage が付くので、行ごとに足すと約 2 倍になる。主標本の中央値は message ごと 33,928、行ごと 71,625。人が planner と見た「中央値 約 14.6 万」とは数え方か標本が違うとみられる（どちらかは確かめていない）。順位の指標には影響しない）。
- model の時間: 各 assistant message について、その直前の main thread の非 assistant の行から、その message の最後の行までの秒数の合計。出力 token との Spearman は 0.99。
- 変更行数: squash commit の `git show --numstat` の追加 + 削除。
- 手戻り: `stats` の `resumes` が 1 以上か、`review_verdict` が `revise` / `concern`（標本に `revise` は無く、review は `concern` だけ）。原因は `resume_started` の reason で、衝突（conflict）・外からの kill（killed）・integrate の検証の失敗（verification）に分けた。integrate の検証の失敗は `verification_command` の event（`integrate-` の log、exit code ≠ 0）でも数えた（90 件中 12 件。うち 2 件（task 75・94）は resume も concern も無く、手戻りに数えていない）。

### 指標

- Spearman: 予測値（規則は score、LLM は `expected_output_tokens`）と出力 token・model の時間の順位相関。90% 区間は 2,000 回の bootstrap。
- 重い上位 3 分の 1: 実測の出力 token の上位 21 件を、予測の上位 21 件がどれだけ当てるか（件数が同じなので precision = recall）。LLM の予測は 110,000 のような丸い値で同点が多いので、境目の同点は期待値で数えた（同点の各 run が同じ確率で上位に入るとする）。LLM は `size = L` を重いと予測したとみなした precision / recall も出した。
- 手戻りの AUC: `rework_probability`（規則は rework score）で、手戻りあり / なしを分ける AUC。「task に由来する手戻り」は衝突か kill だけの手戻り 13 件を除いた 77 件（陽性 18 件）での AUC。
- コスト: 1 回の予測の入力 token（cache の読み書きを含む）・出力 token（thinking を含む）・USD（`total_cost_usd`）と、呼び出しの wall time の中央値。

## 結果

### 重さ（主標本 65 件）

| 担い手 | 入力 | Spearman（出力 token） | 90% 区間 | Spearman（model の時間） | 上位 3 分の 1 の P = R | `L` の precision / recall（件数） | size の Spearman |
|---|---|---|---|---|---|---|---|
| 規則（手） | a | 0.28 | 0.08〜0.47 | 0.31 | 0.48 | — | — |
| 規則（手） | b | 0.31 | 0.11〜0.50 | 0.34 | 0.57 | — | — |
| 規則（LOO 回帰） | a | 0.67 | 0.54〜0.77 | 0.66 | 0.71 | — | — |
| 規則（LOO 回帰） | b | 0.71 | 0.58〜0.79 | 0.71 | 0.71 | — | — |
| Haiku 4.5 | a | 0.68 | 0.52〜0.79 | 0.66 | 0.74 | 0.75 / 0.71（20） | 0.64 |
| Haiku 4.5 | b | 0.66 | 0.50〜0.77 | 0.64 | 0.71 | 0.71 / 0.81（24） | 0.67 |
| Sonnet 5 | a | 0.79 | 0.69〜0.87 | 0.76 | 0.82 | 0.57 / 1.00（37） | 0.74 |
| Sonnet 5 | b | 0.86 | 0.78〜0.91 | 0.84 | 0.82 | 0.64 / 1.00（33） | 0.81 |
| Opus 5.5 low | a | 0.87 | 0.79〜0.92 | 0.85 | 0.83 | 0.54 / 1.00（39） | 0.77 |
| Opus 5.5 low | b | 0.87 | 0.79〜0.93 | 0.86 | 0.82 | 0.57 / 1.00（37） | 0.77 |
| Opus 5.5 high | a | 0.89 | 0.82〜0.93 | 0.87 | 0.84 | 0.58 / 1.00（36） | 0.80 |
| Opus 5.5 high | b | 0.91 | 0.86〜0.94 | 0.89 | 0.90 | 0.57 / 1.00（37） | 0.79 |

参考（事前には分からない値）: 変更行数と出力 token の Spearman 0.75。task id（時期）と出力 token 0.16。主標本の出力 token の中央値は runtime 56,223（37 件）、plugin 28,223（4）、test/build 27,099（7）、docs 22,849（13）、other 18,471（4）。

手の規則が弱いのは、重さの score に強い特徴（依存の数・依存元の要約の長さ）を入れず、一部の重みの向きが外れていたため。1 つの特徴ずつの Spearman は、依存の数 0.47〜0.49、依存元の要約の長さ 0.53、runtime 0.44、llvm-cov 0.44、acceptance の項目数 0.31、本文の長さ 0.19 で、test/build（−0.18）と名指すファイルの数（−0.24）はむしろ軽い側に効いていた（手では正の重みを付けた）。

### Spearman の差（同じ run での paired bootstrap、90% 区間）

| 比較 | 差 | 90% 区間 |
|---|---|---|
| 規則（手） b − a | +0.03 | +0.01〜+0.07 |
| 規則（LOO） b − a | +0.03 | −0.02〜+0.09 |
| Haiku b − a | −0.02 | −0.16〜+0.12 |
| Sonnet b − a | +0.07 | −0.02〜+0.16 |
| Opus low b − a | +0.01 | −0.03〜+0.04 |
| Opus high b − a | +0.03 | 0.00〜+0.06 |
| Haiku (b) − Sonnet (b) | −0.20 | −0.33〜−0.09 |
| 規則（LOO）(b) − Sonnet (b) | −0.15 | −0.25〜−0.07 |
| Opus low (b) − Sonnet (b) | +0.02 | −0.05〜+0.09 |
| Opus high (b) − Sonnet (b) | +0.06 | 0.00〜+0.11 |

### 手戻り（主標本 + 補標本 90 件、手戻り 31 件）

手戻り 31 件の原因: 衝突だけ 10、衝突 + concern 5、衝突 + 検証の失敗 3、衝突 + kill 1、検証の失敗だけ 7、kill だけ 2、concern だけ 3。

| 担い手 | 入力 | AUC（手戻り） | AUC（task に由来、77 件中 18 件） | AUC（重さの予測値で） |
|---|---|---|---|---|
| 規則（手） | a | 0.52 | 0.51 | 0.52 |
| 規則（手） | b | 0.54 | 0.54 | 0.52 |
| Haiku 4.5 | a | 0.62 | 0.65 | 0.58 |
| Haiku 4.5 | b | 0.54 | 0.57 | 0.55 |
| Sonnet 5 | a | 0.61 | 0.63 | 0.60 |
| Sonnet 5 | b | 0.65 | 0.64 | 0.68 |
| Opus 5.5 low | a | 0.69 | 0.68 | 0.66 |
| Opus 5.5 low | b | 0.66 | 0.64 | 0.66 |
| Opus 5.5 high | a | 0.68 | 0.68 | 0.65 |
| Opus 5.5 high | b | 0.63 | 0.60 | 0.65 |

### 1 回の予測のコストと時間（90 件の平均、時間は中央値）

| 担い手 | 入力 | 入力 token | 出力 token | USD | 時間（秒） |
|---|---|---|---|---|---|
| 規則 | a / b | 0 | 0 | 0 | < 0.01 |
| Haiku 4.5 | a | 2,981 | 3,858 | 0.022 | 39.8 |
| Haiku 4.5 | b | 3,824 | 3,967 | 0.025 | 41.1 |
| Sonnet 5 | a | 3,622 | 365 | 0.018 | 6.2 |
| Sonnet 5 | b | 4,757 | 505 | 0.024 | 9.2 |
| Opus 5.5 low | a | 3,559 | 182 | 0.032 | 5.1 |
| Opus 5.5 low | b | 4,694 | 192 | 0.041 | 5.2 |
| Opus 5.5 high | a | 3,559 | 299 | 0.034 | 6.5 |
| Opus 5.5 high | b | 4,694 | 322 | 0.043 | 6.5 |

Haiku は `claude -p` の既定で thinking を使い、出力の大半が thinking だった。

呼び出しの総数は 720 回（4 つの LLM の候補 × 90 件 × (a)(b)）、入力 2,852,000 token・出力 872,101 token・合計 21.58 USD。このほか、動作確認の呼び出しが 4 回ある。

### token の見込みの値（(b)、主標本）

| 担い手 | 予測の中央値 | run ごとの予測 / 実測の比の中央値（中央値どうしの比） | log10 の平均絶対誤差 | size S / M / L | nature の最多 |
|---|---|---|---|---|---|
| Haiku 4.5 | 35,000 | 1.00（1.03） | 0.23 | 5 / 36 / 24 | implementation 40 |
| Sonnet 5 | 75,000 | 2.32（2.21） | 0.39 | 6 / 26 / 33 | design_judgment 30 |
| Opus 5.5 low | 110,000 | 2.57（3.24） | 0.40 | 6 / 22 / 37 | implementation 37 |
| Opus 5.5 high | 90,000 | 2.67（2.65） | 0.41 | 6 / 22 / 37 | implementation 38 |

## 限界

- **件数が少ない**: 重さは 65 件、手戻りは 31 件（task に由来するものは 77 件中 18 件）。Sonnet と Opus の差、(a) と (b) の差は 90% 区間が 0 をまたぐか接していて、順序までは言えない。言えるのは「手の規則 < Haiku ≈ LOO 規則 < Sonnet ≲ Opus」と「手戻りはどれも弱い」まで。
- **期間が短く、1 つの repository だけ**: 2026-09-23〜26 の 4 日間の、この repository の run だけ。worker の手順（AGENTS.md の worker の test の絞り方など）がこの期間にも変わっていて、同じ本文でも時期で重さが変わりうる（task id との Spearman は 0.16 と小さい）。
- **(a) の多くは plan review の前の時代**: 主標本の 53 / 65 件は `task_submitted` が無く ready への遷移を (a) の切断点にした。そのため (a) と (b) で本文の文面が違うのは 1 件、依存が違うのは 6 件しかなく、(a) と (b) の差はほぼ main と依存元の要約の差になっている。
- **手戻りの補標本の抽出が揃っていない**: 手戻りの AUC の陰性は変更行数（結果）で層別した主標本から、陽性は母集団の全件から来ている。変更行数と手戻りに関係があれば AUC がずれうる。
- **main の見せ方が浅い**: 名指すファイルの行数と番号だけで、中身は見せていない。本番の plan review job や claim の予測が repository を読めば、今回より良くなる余地がある（特に Haiku と規則）。
- **出力 token の目安を母集団から書いた**: prompt の「中央値 3.5 万」は母集団の値で、全候補に同じに見せた。個々の run の結果ではないが、値の水準の比較には効く（順位の指標には効かない）。
- **LOO 規則は同じ標本で特徴を選んだ**: 特徴の選び方と ridge の強さはこの標本を見て決めたので、LOO でも少し楽観的。
- **model / effort の効果は測っていない**: worker は全 run が Opus 5.5 の既定の effort なので、「軽い task を軽い model で回すと重さや手戻りがどう変わるか」は分からない。C を進めるなら、予測を記録したうえで一部の task だけ model / effort を変えて比べる必要がある。
- **LLM の非決定性**: 各入力を 1 回だけ予測した。同じ入力での揺れは測っていない。
- **model が dagq を知っているか**: どの model も知識の期限より後の repository なので、中身を覚えている可能性は低い。
