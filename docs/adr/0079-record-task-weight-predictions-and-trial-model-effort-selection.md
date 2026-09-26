---
id: adr-0079
type: adr
title: plan reviewでtaskの重さの予測を記録し、限定の試しでworkerのmodel / effortを選び、taskに由来する失敗で段上げする
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - performance
  - worker
  - planning
  - measurement
related:
  - plan-spike-predictor-replay
  - adr-0027
  - adr-0044
  - adr-0049
  - design-supervisor-lifecycle-kpi
---

# ADR-0079: plan reviewでtaskの重さの予測を記録し、限定の試しでworkerのmodel / effortを選び、taskに由来する失敗で段上げする

## Context

workerのmodelとeffortをtaskごとに選ぶ案（対策C）の前提として、task 553のスパイク（[docs/plans/spike-predictor-replay.md](../plans/spike-predictor-replay.md)）が、選ぶ時点で手元にある情報からtaskの重さと手戻りをどれだけ予測できるかを、予測の担い手ごとに測った。主標本は着地した193 runから層別に選んだ65件（手戻りは補標本を足した90件）。

- **重さの順位はLLMが本文から十分に予測できる。** 出力tokenとのSpearmanは、plan reviewのときの入力(a)でSonnet 5が0.79・Opus 5.5 lowが0.87・highが0.89、claimのときの入力(b)でSonnet 0.86・Opus low 0.87・high 0.91。後から分かる変更行数（0.75）を上回る。Haiku 4.5は0.68 / 0.66で、Sonnet (b)との差は−0.20（90%区間−0.33〜−0.09）。しかも`claude -p`のHaikuはthinkingで1回約3,900 token・約40秒かかり、Sonnet（6〜9秒・約0.02 USD）より遅い。
- **claimのときの予測はplan reviewのときより少し良いだけ。** (b)−(a)はSonnet +0.07（−0.02〜+0.16）、Opus high +0.03（0.00〜+0.06）、Opus low +0.01。(b)で増えるのは主に依存元のreceiptの要約で、本文の文面が(a)と(b)で違ったのは65件中1件だけ。
- **手戻りはどの担い手でもほとんど予測できない。** AUCは0.52〜0.69。手戻り31件のうち19件は並行する着地とのrebaseの衝突、3件は外からのkillを含み、どちらもtaskの本文からは決まらない。衝突とkillだけの手戻りを除いた「taskに由来する手戻り」（integrateの検証の失敗のresumeかreviewのconcern）は77件中18件（23%）で、そのAUCも0.51〜0.68。
- **tokenの見込みの値は2〜3倍に偏る。** 予測 / 実測の比の中央値はSonnet 2.3倍、Opus 2.6〜2.7倍で、promptに目安を書いても従わない。`size = L`も多すぎる（65件中33〜39件）。順位は使えるが、値の閾値は使えない。
- **1回の予測は0.02〜0.04 USD・10秒以内**で、worker 1 run（出力tokenの中央値 約3.4万、modelの時間の中央値 約6分）に比べて小さい。
- **modelとeffortを変えたときのworkerの効果は測っていない。** 標本のworkerは全turnが`claude-opus-5-5`だった。今のworkerのeffortは、runtimeが指定していないClaude Codeの既定のmediumである。workerのtranscript（`~/.claude/projects/…-runs-<run_id>-worktree/<run_id>.jsonl`）のassistantの行には`"model":"claude-opus-5-5"`と`"effort":"medium"`（`"perTurnEffort":"medium"`）が記録されていて、2026-09-26に確かめた1本では70 messageすべてがそうだった。

スパイクの「Cを進めるなら」はclaimのときにSonnetで予測し直して選ぶ案だったが、2026-09-26の夜に人がplannerと基準を詰め、goal 51の決定で上書きした（予測はplan reviewの1回だけ、試しは限定、段上げはtaskに由来する失敗だけ）。あわせて、worker以外のアクター（plan reviewのjob・reviewのjob・triage / 復旧のjob・observer・runtimeが立てるplanner・人が`dagq plan`で開くplanner）のmodel / effortの進め方も決めた。

worker以外のアクターの数値: 12:00以降のplan review 43件の稼働の合計は11分（中央値14秒）で、同じ期間のworker（5.8時間）の約3%。effortを上げる費用は小さい。一方で計画の判断（重複・食い違い・フォローアップの採否）は、後の多くのrunに波及する。

## Decision

### 1. 最適化の軸は速さ、守る指標はtaskに由来する手戻りの率

- **最適化の軸**: 速さ（リードタイムと1時間あたりの完了数）。費用は大きく増えない範囲で見る。
- **守る指標**: taskに由来する手戻りの率。+5ポイントまでの悪化を許し、超えたら戻す。goal 51で人が見た数はスパイクの標本の23%（77件中18件、上限28%）だが、この標本は手戻りを補標本で多く取り、衝突とkillだけの13件を分母から除き、`revise`を含まない（標本に無かった）ので、率の基準値には使えない（母集団では193 run中18件で約9%）。そのため+5ポイントは絶対の値ではなく差で当てる。試し（決定4）では同じ期間のcontrolとの差（treatment − control）、それ以外の変更では変更の前の同じ数え方の率（記録から`stats`で出し直す）との差で見る。
- **数え方**: runを単位にし、次のどれかが1つでもあれば「taskに由来する手戻りあり」と数える。
  - integrateの検証の失敗（`integrate-<attempt>-verify-N`のコマンドの非0終了）によるresume
  - reviewのverdictが`concern`
  - reviewのverdictが`revise`
- **数えないもの**: rebaseの衝突によるresume（`conflict`）と、sessionが外からkillされたことによるresume（`killed`）。どちらも並行度・着地の順・hostで決まり、taskの本文では決まらない。衝突やkillと上の3つの両方があったrunは、上の3つがあるので手戻りありに数える。

### 2. 予測はplan reviewのjobが出力に含めてtaskごとに1回記録し、claimでは予測しない

- **担い手**: plan reviewのjob（[plan review](../design/supervisor-lifecycle/plan-review.md)）が、verdictのJSONに、proposalのsubmittedのtaskごとの予測を含めて出す。形はスパイクと同じ`{"task_id", "size": "S" | "M" | "L", "nature": "mechanical" | "implementation" | "design_judgment" | "investigation", "uncertainty", "expected_output_tokens", "rework_probability", "reason"}`の配列。予測のために別のmodelを呼ばない（plan reviewのjobは本文とrepositoryを読んでいるので、そのjobの判断に相乗りするのが一番安く、スパイクではOpusの予測はSonnetと同等以上だった）。
- **claimでは予測しない**: (b)−(a)の差は小さく（Sonnet +0.07・Opus high +0.03で、区間は0をまたぐか接する）、claimの経路に外部呼び出しの遅れと失敗を足す価値がない。
- **記録の回数**: plan review 1回につきtaskごとに1回（claimやresumeでは足さない）。記録するのはverdictを適用したplan review（`pass` / `revise` / `concern`のどれでも）で、出し直しや`reopen`でplan reviewにかけ直したtaskは新しい予測で上書きせず追記し、読むときは最後の記録を使う。`ready --bypass-review`でreadyになったtaskには予測が無い。
- **記録先はrun_eventsのevent**（`task_weight_predicted`、taskに付け、`proposal_id`・`plan_review_id`・予測のJSON・予測したjobのmodel / effortを持つ）。専用のtableは作らない。理由: (1) 読む側の`stats`と`kpi`はrun_eventsを走査する純粋関数で、同じ走査の中でtaskの最後の予測をrunに結び付けられる。(2) migrationが要らず、並行するrunとmigration番号を取り合わない。(3) 予測は追記だけで、書き換えや問い合わせの索引が要らない。
- **読み方**: `stats --full`のrunごとの行に、そのtaskの最後の予測（size・nature・expected_output_tokens・rework_probability）と、runの実績（出力token・modelの時間・resumeの回数と理由・reviewのverdict・taskに由来する手戻りの有無）を並べて出す。試しの比較（決定6）は`kpi`の層（`group=`・`model=`・`effort=`・`nature=`）で読む。予測の精度（Spearman・百分位の当たり）は`stats`の行から計算できれば足り、runtimeに集計を持たない。
- **予測の失敗はplan reviewを止めない**: verdictに予測が無い、JSONとして不正、taskが足りない・余る、値が範囲外のときは、verdictの適用はそのまま行い、予測は記録しない（`plan_review_finished`に`prediction_error`を残す）。予測が無いtaskは試しの対象にしない（決定4）。予測のフィールドはverdictの未知フィールドの拒否の対象外にして、予測の形の誤りでverdict全体が失敗にならないようにする。

### 3. 既定の設定: workerはOpus 5.5・effort mediumで、runtimeが明示して渡し、使った値を記録する

- 今の既定（Opus 5.5・effort medium）を変えない。試し（決定4）と段上げ（決定5）が働かないtaskの最初の起動は、Opus 5.5・mediumのまま。
- runtimeは、workerの起動・resume・reviseのたびに、modelとeffortを明示してproviderに渡す（Claude Codeの既定やユーザー設定に依らない）。reviseは生きているsessionに差し戻す（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）ので、段上げでmodel / effortが変わるときはsessionの中で切り替えてから差し戻しを送る。切り替えられなかったときは段上げせずに差し戻し、その理由を記録する。
- 使った値を記録する: workerのsessionを開くevent（claimの後の最初の起動は`run_claimed`、resumeは`resume_started`、reviseは差し戻しの配送のevent。足りなければ実装のtaskがsessionの起動のeventを新しく足す）に、`model`・`effort`・`group`（`control` / `treatment` / なし）・段上げしたなら`escalated_from`と`escalation_reason`を持たせる。

### 4. 限定の試し: 既定は無効、mechanicalかつ下位3分の1のtaskだけを交互に割り当てる

- **設定**: `dagq.toml`で有効 / 無効を切り替える。既定は無効で、有効にするのは人の判断。例（書式の細部は実装のtaskがdesign文書に書く）:

  ```toml
  [worker.trial]
  enabled = false
  window = 60
  ```

- **対象**: claimのとき、taskの最後の予測の`nature`が`mechanical`で、かつその`expected_output_tokens`が、そのtask自身を除く直近N件の予測（記録の新しい順、taskごとに最後の1件）の下位3分の1（33.3百分位以下）に入るtask。値ではなく百分位で決めるのは、予測の値が2〜3倍に偏り、時期で水準も動くため。
- **N = 60**。理由: 下位3分の1が20件になり、1件の予測の揺れで境目が大きく動かない。スパイクの主標本（65件）と同じ程度で、今の流量（4日で約190 run）なら1日強で入れ替わり、手順の変化に遅れすぎない。
- **予測がN件に満たない間は、試しの対象にしない**（全taskをcontrolの設定のOpus mediumで起動し、群は記録しない）。予測が無いtask（bypass、予測の失敗）も対象にしない。
- **割り当て**: 対象になったtaskを、最初のclaimの順に`control`（Opus 5.5・medium）と`treatment`（Sonnet 5・medium）に交互に割り当てる。群はtaskに固定し、同じtaskのresume・revise・retry（新しいrun）でも変えない。群とmodel / effortは決定3のとおり記録する。
- **対象外のtaskは変えない**: 対象外のtaskの最初の起動はOpus 5.5・mediumのまま。Haikuは使わない。
- **手戻りの見込み（`rework_probability`）は記録だけで、選択の根拠にしない**（AUC 0.51〜0.68で、選択に使える精度がない）。

### 5. 段上げ: taskに由来する失敗のresume / reviseで1段上げ、衝突とkillでは上げない

- 段は **Sonnet 5 medium → Opus 5.5 medium → Opus 5.5 high → Opus 5.5 xhigh** で、xhighが上限（上限に達したら同じ段のまま）。treatmentはSonnet 5 mediumから、それ以外（controlと対象外）はOpus mediumから始まる。
- **上げる**: 決定1で手戻りに数える失敗の後の起動、つまりintegrateの検証の失敗によるresume、reviewの`revise`による差し戻し、`concern`の`approve_landing`に人が`send_back`と答えた差し戻し。上げた段はそのtaskの以後のrun（retryを含む）に引き継ぐ。
- **上げない**: rebaseの衝突によるresume、外からのkillによるresume。どちらもtaskの難しさの信号ではない。それ以外のresumeの理由（`evidence_missing`・`scope_violation`・`migration_number_taken`・`prompt_waiting`など）も、決定1で手戻りに数えないので上げない。
- **試しの有無に関係なく全taskに効く**: 試しが無効でも、taskに由来する失敗の後はOpus medium → high → xhighと上げる。段上げの理由（どのeventによるか）を決定3のとおり記録する。

### 6. 判定と止める条件は人とplannerが決める

- 1群45件前後で、controlとtreatmentの速さ（リードタイム・workの時間・modelの時間）と、taskに由来する手戻りの率（決定1の数え方、段上げの後のrunの失敗もそのtaskの手戻りに含める）を比べる。
- treatmentが速く、手戻りの率の差（treatment − control）が+5ポイント以内なら、対象（mechanicalかつ下位3分の1）の最初の起動を全面的にSonnet 5 mediumにする。
- 45件は+5ポイントの差を統計的に見分けられる数ではない（基準の率が10〜20%なら差の標準誤差は6〜9ポイント）。判定は点推定の差で行い、人とplannerは速さの差の大きさと合わせて読む。対象はmechanicalかつ下位3分の1のtaskだけなので、集まるまで数日より長くかかりうる。集まる速さは実装の後に`stats`で見る。
- 途中で大きく超えたら止める。目安は、treatmentが15件以上たまった時点で差が+15ポイント以上。
- 判定するのは人とplannerで、runtimeは自動で止めたり全面適用したりしない。止めるのは`dagq.toml`の設定を無効に戻すこと、全面適用は別のtaskで既定を変えること。

### 7. worker以外のアクター: 先に計測し、役割ごとの設定を入れ、1段ずつ比べて頭打ちで止める

対象はplan reviewのjob・reviewのjob・triage / 復旧のjob・observer・runtimeが立てるplanner・人が`dagq plan`で開くplanner。

- **(a) 先に計測する**: これらのsessionが使ったmodel / effortを、sessionの区間のevent（`stats`の`sessions.by_kind`の元）に記録し、今の既定（medium）の基準値を1週間程度ためる。
- **(b) 役割ごとの設定**: 役割ごとのmodel / effortを`dagq.toml`で設定できるようにする（例: `[roles.plan_review] model = "claude-opus-5-5"`、`effort = "medium"`。書式の細部は実装のtaskが決める）。既定は今と同じOpus 5.5・mediumで、入れただけでは挙動を変えない。runtimeはworkerと同じく明示して渡して記録する。
- **(c) 差し戻しで開き直すplannerは1段上げる**: plan reviewの`revise`でplannerを開き直す（runtimeが立てる）ときは、そのplannerのeffortを設定の値から1段上げる（medium → high → xhigh、xhighが上限。modelは変えない）。差し戻しは計画が一度外れた信号なので、そこだけ考える量を増やす。生きているplannerへ配送するときも、決定3と同じくsessionの中で切り替えてから送り、切り替えられなければ上げずに理由を記録する。
- **(d) 1段ずつ比べて頭打ちで止める**: 基準値がたまったら、plan reviewとruntimeが立てるplannerをhighに固定してmediumと比べ、効果があればhighを既定にする。次の段（xhighや、proposalの特徴で事前に選ぶ動的な規則）はhighの実績を基準に同じように比べ、効果が頭打ちになったら止める。(d)の実際の変更は、人とplannerが判断してから別のtaskで行う。
- **計画の品質の指標**（proposalを単位にし、判断したsessionのmodel / effortと並べて読む）:
  - plan reviewの`revise`の率（proposalごとの差し戻しの回数）
  - readyになった後に重複としてcancelされたtaskの件数（`task_canceled_as_duplicate`のうち、元のtaskがreadyを経たもの）
  - フォローアップの採用率（follow-upのdraftがadoptされた割合）と、採用後にcancelされた件数
  - その計画から生まれたtaskの、taskに由来する手戻りの率（決定1）
- **proposalの特徴**（層に使う）: 出どころ（人が開いたplanner / runtimeが立てたplanner / follow-up / observer）、フォローアップの深さ（follow-upのfollow-upなら2）、`dagq related`の点数（既存のtaskとの近さの最大値）、差し戻しの回数。plan reviewの開始のevent（`plan_review_started`）に記録する。
- **読み方**: `kpi`の層に`model=`・`effort=`と上の特徴（`origin=`・`follow_up_depth=`・`related=`（低 / 中 / 高）・`revise_count=`）を足し、計画の品質の指標を層別して読む。

## Alternatives

- **claimのときにSonnetで予測し直して選ぶ（スパイクの「Cを進めるなら」2）**: (b)−(a)の差は小さく区間が0をまたぐか接し、claimの経路に外部呼び出しの遅れと失敗の扱いを足すことになる。採らない。
- **予測を別のSonnetの呼び出しで作る**: 1回約0.02 USDで安いが、呼び出しと失敗の扱いが1つ増える。plan reviewのjobの出力に含めれば、repositoryを読んだOpusの判断に相乗りできる。採らない。
- **予測を専用のtableに記録する**: 問い合わせは楽になるが、migrationが要り、`stats` / `kpi`の走査とは別の読み口になる。追記だけなのでeventで足りる。採らない。
- **Haiku 4.5を軽いmodelとして使う**: 順位の予測でSonnetに劣り、`claude -p`ではthinkingで遅い。予測にもworkerにも使わない。
- **予測の値（token）で閾値を決める**: 値は2〜3倍に偏り、promptで直らない。百分位にした。
- **手戻りの見込みでmodel / effortを選ぶ**: AUCが0.51〜0.68で、選択に使うと速さを失うだけになりうる。記録だけにした。
- **試しを全taskに広げる、または既定で有効にする**: workerのmodel / effortの効果は一度も測っておらず、重いtaskでSonnetにして手戻りが増えると取り返しが大きい。一番影響の小さいmechanicalかつ下位3分の1から、人が有効にして始める。
- **衝突とkillでも段上げする**: taskの難しさと関係のない理由で重い設定に上がり、費用が増えるうえ、段上げの効果の読みが濁る。採らない。
- **worker以外のアクターをすぐhighにする**: 費用は小さいが、基準値が無いと効果を判断できない。先に計測し、1段ずつ比べる。

## Consequences

- goal 51の実装は、(1) plan reviewのjobの予測の出力と`task_weight_predicted`の記録、`stats`のrunごとの予測と実績、(2) worker・resume・reviseでのmodel / effortの明示と記録、(3) `dagq.toml`の試しの設定・対象の判定・交互の割り当て、(4) 段上げ、(5) worker以外のアクターの計測と役割ごとの設定と差し戻しの1段上げ、(6) 計画の品質の指標とproposalの特徴の`kpi`の層、のtaskに分けて進める。実装のtaskが該当するdesign文書（plan-review.md・kpi.md・stats.md・run environment）に書式を書く。
- 試しを有効にしない限り、最初の起動は今と同じOpus medium。挙動が変わるのは段上げ（taskに由来する失敗の後にOpus highへ上がる）だけで、その分の費用は増える。
- plan reviewのjobの出力が長くなり、1回の時間とtokenが少し増える（予測1件あたりスパイクのOpusで出力200〜300 token程度）。
- 予測の失敗はplan reviewを止めないので、予測の抜けは試しの対象の減少としてだけ現れる。抜けの率は`plan_review_finished`の`prediction_error`で見る。
- 判定（決定6）と、worker以外のアクターの既定の変更（決定7の(d)）は人とplannerの判断で、runtimeは自動で変えない。
- 標本は1つのrepositoryの4日分で、modelを変えたときのworkerの効果は測っていない。試しの結果が出るまで、Sonnetのworkerが手戻りを増やさないという前提は仮説のまま。
