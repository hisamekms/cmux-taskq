---
id: adr-0045
type: adr
title: 固定バイナリをbuild識別子で見分け、queueを開いただけではmigrateせず、互換の範囲のschemaを受け入れ、supervisorを待たずに引き継ぎで入れ替え、up --auto-updateで着地のたびに自動で更新する
status: superseded
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
superseded_by: adr-0073
superseded_on: 2026-09-26
supersedes:
  - adr-0014
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - persistence
  - operations
  - release
related:
  - adr-0003
  - adr-0005
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0014
  - adr-0026
  - adr-0030
  - adr-0034
  - adr-0035
  - adr-0040
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0045: 固定バイナリをbuild識別子で見分け、queueを開いただけではmigrateせず、互換の範囲のschemaを受け入れ、supervisorを待たずに引き継ぎで入れ替え、`up --auto-update`で着地のたびに自動で更新する

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0073](0073-kind-additions-are-compatible.md)を読む。

## Context

[ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md)（2026-09-22）は、固定バイナリの更新を「ファイルを置き換えて`up`」の2手にした。`supervise`プロセスが自分の`CARGO_PKG_VERSION`を`supervisors.binary_version`に書き、`up`はliveな登録のversionが自分と違えば、その登録を`down --wait`と同じ手順でdrainして（走っているrunの完了を待って）から、自分のversionのsupervisorを1つ立てる。動機はtask 20（2026-09-22）の事故で、`target/debug`のバイナリで本番のqueue DBを開いただけでmigrationが走り、古いschemaの固定バイナリのsupervisorと配下のrunが`unsupported queue schema version`で止まった。

2026-09-25の時点で、この仕組みには4つの問題がある。

1. **drainが長い。** drainは走っているrunの終わりを待つ。runはClaude sessionなので数分から数時間かかり、その間`up`は返らず、新しいclaimも止まる。`up --no-wait`は走っているrunがあれば何もせずに止まるだけで、待たずに入れ替える手段ではない。
2. **versionで見分けられない。** 判定は`CARGO_PKG_VERSION`で、リリース・この repository のドッグフーディング・開発中のビルドが全部同じ`0.3.0`を名乗る。versionを上げずにビルドし直したバイナリは`up`にreuseされ、`down --wait`が要る（2026-09-25に実際に要った）。ADR-0014自身がConsequencesでこの穴を書き、build hashを名乗る案を「versionという語の意味が2つになる」として退けていた。
3. **開いただけでmigrateし、新しいschemaを拒む。** バイナリはqueueを開くたびに未適用のmigrationを適用し、自分より新しい`user_version`を拒否する（[persistence](../design/persistence.md)のDatabase setup and migrations）。開発中のバイナリで本番のqueueを開くと黙ってmigrateされ、古いバイナリのsupervisorと、run の wrapper（claim時のバイナリのコピー`runs/<id>/runner`）が止まる。今は[AGENTS.md](../../AGENTS.md)の「本番のqueueには固定バイナリだけを使う」という運用ルールで避けているだけで、ADR-0014もこれはruntimeが防げない部分として残した。
4. **cpの上書きが走行中のプロセスを殺しうる。** 動いているバイナリのファイルを`cp`で上書きすると、macOSではページとコード署名の不整合で、そのバイナリで走っているプロセスがkillされうる。

2026-09-25のplannerとの対話で、ユーザーはgoal 32としてこれらを解くと決め、決定をgoalのconstraintsに書いた。解決後は、runtimeを変えるcommitが着地するとsupervisorが自動でビルドして入れ替え、待ちは生じない。人も`dagq install`で随時入れ替えられる。versionでリリースと開発のビルドが区別でき、開発中のバイナリが本番のqueueを開いても壊れない。

ADR-0014の決定を変えるので、[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)に従い、このADRはADR-0014のまだ生きている決定（`binary_version`を登録するプロセス自身が書く、判定はliveな登録だけを対象にする、起動できないと分かる条件を止める前に確かめる、など）を書き直して引き継ぎ、ADR-0014を丸ごと置き換える。

## Decision

### build識別子

1. **versionの付け方。** リリースは`X.Y.Z`。mainはリリースの直後に次の開発版`X.Y.Z-dev`へ上げる。次に上げる桁はリリースのときに決めてよく、今は`0.4.0-dev`にする。release skillの手順に、tagを切るときに`-dev`を外す段と、tagの後で次の開発版へ上げる段を足す。
2. **build識別子。** ビルド時にgitのcommitを埋め込み、versionにpre-release（`-dev`）があるビルドは`X.Y.Z-dev+<commit>`を名乗る（`<commit>`はcommitの短縮しないSHA）。ビルドしたworktreeにcommitされていない変更があれば`+<commit>.dirty`にする（SemVerのbuild metadata）。`-dev`の無いversion（リリース）は`X.Y.Z`だけを名乗る。gitが無い、またはrepositoryの外でビルドされた開発版（crates.ioのsourceからの`-dev`のビルドなど）は`X.Y.Z-dev+unknown`とする。`dagq --version`、`supervisors.binary_version`、`status` / `doctor`の`binary_version`、`up`の結果の`version`はすべてこのbuild識別子を出す。pluginのlauncherがバイナリとの互換を見るのは従来どおり`X.Y`の部分だけで、pre-releaseとbuild metadataは見ない。
3. **判定はbuild識別子の全体で行う。** 入れ替えが要るかは、build識別子の文字列が一致するかで決める。SemVerの優先順位はbuild metadataを無視するが、ここでは順序ではなく同一性を見るので、build metadataまで含めて比べる。どちらが新しいかは判定しない（古いバイナリへの`install --rollback`も同じ経路で入れ替わる）。リリースを使う利用者にとってはリリースごとにしか変わらず、開発版ではcommitごとに変わる。本番と開発で別の仕組みは持たない。
4. **`binary_version`は登録するプロセス自身が書く**（ADR-0014の決定1を引き継ぐ）。`register_supervisor`が自分のbuild識別子を書き、引き継ぎ（決定10）でexecした新しいプロセスも自分の識別子で書き直す。どのビルドで動いているかを知っているのはそのプロセスだけなので、`mode` / `workspace_id`（`up`が書く）とは書き手が違う。列の無かった頃のバイナリが書いた行（null）は「どのbuild識別子とも違う」に含める。

### schemaとmigration

5. **queueを開いただけではmigrateしない。** migrateするのは明示したコマンドだけにする: `dagq migrate`と、入れ替えの手順（`dagq install`、自動更新、`up`の入れ替え）の中。`init`は例外で、新しいqueueを最新のschemaで作る（既存のqueueに対する`init`の再実行はmigrateしない）。バイナリが要るschemaよりDBが古ければ、そのバイナリはDBを書き換えずに、`dagq migrate`を案内するerrorで止まる（`migrate`、`init`、`--version`、`doctor`のうちschemaを報告する部分は動く）。
6. **migrationごとに互換を宣言する。** 各migrationは、そのmigrationより前のschemaしか知らないバイナリがmigrate後のDBを読み書きしても壊れないか（**互換**か**非互換**か）を、migrationと同じ場所でrepositoryに宣言する。互換と宣言してよいのは、表を足す、既定値のある列かnull可の列を足す、indexを足す、のように、古いバイナリが知らない表と列を無視しても読み書きが成り立つ変更だけである。既存の列に新しい値（statusのCHECKに値を足すなど）を入れうる変更、表の作り直し、列の削除・改名・意味の変更は非互換とする。古いバイナリは知らない値を読めないからである。
7. **DBは受け入れる下限を持つ。** DBは`user_version`に加えて、「これより古いschemaのバイナリは拒否する」下限を持つ。SQLiteのheaderには`user_version`と`application_id`しか無いので、下限は下限の仕組みを入れるmigrationが作る1行のmeta表に置き、バイナリは`user_version`を判定する前にこれを読む（meta表が無ければ下限は`user_version`そのものとみなす）。下限は、適用済みのmigrationのうち非互換と宣言された最後のもののschema versionで、`migrate`が`user_version`と同じトランザクションで書く。バイナリは、自分の知る最新のschema version（`S_bin`）がDBの下限以上なら、DBの`user_version`が`S_bin`より新しくても受け入れ、自分の知らない表と列には触れずに動く。下限より古ければ、従来どおり`unsupported queue schema version`で拒否する（文面に下限と`install`の案内を載せる）。下限の仕組みを入れるmigrationそのものは、この決定を知らない既存のバイナリがどのみち新しい`user_version`を拒むので、非互換として扱う。それより前のmigrationはすべて非互換とみなす。
8. **非互換のmigrationだけがdrainを要する。** 非互換のmigrationを適用すると、走っているrunのwrapper（claim時のバイナリのコピー`runs/<id>/runner`）と古いsupervisorがDBを開けなくなる。そこで`dagq migrate`は、非互換のmigrationが未適用のうちに生きているsupervisorの登録、走っているrun（`claimed` / `starting` / `running` / `validating` / `integrating`）、またはwrapperが生きているrun（`run_processes`に登録済みで`exited_at`が無くPIDが生きているもの。[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)でworkerのsessionは`awaiting_integration`や`needs_session`でも残る）があれば、何も適用せずに止まる。非互換のmigrationを含む入れ替えは、決定14の人の入口でだけ、ADR-0014と同じdrain（下記の決定14）を経て行う。互換のmigrationは、supervisorとrunが走っているままで適用してよい。
9. **非互換のmigrationの前にDBを退避する。** 非互換のmigrationを適用する前に、SQLiteのbackupでDBの複製をqueueのdirに残す（`<queue dir>/backups/queue-<user_version>-<時刻>.sqlite3`）。互換のmigrationは前のバイナリがそのまま読めるので退避しない。

### 待たない入れ替え（引き継ぎ）

10. **supervisorはsessionの終わりを待たず、自分のpidのまま新しいバイナリをexecする。** 入れ替えの要求（決定14・15・17）を受けたsupervisorは、新しいclaim・provision・integrate・headless jobの開始を止め、進行中の短い処理（claim、provision、validating、integrate、workspaceのclose、`/exit`の送信など、runの状態を1段進める処理）が区切りに着くのを待つ。区切りは短い処理の境目であって、runの終わりではない。走っているworkerのsessionはwrapperの下で動き続け、止めない。headless job（review / triage / plan review / observer）の子プロセスは待たずに止め、exec後のsupervisorがrunの状態からやり直す（どのjobも入力がDBとrunのファイルだけで、やり直しても結果の意味は変わらない）。integrateは`verification_commands`を実行するので、待ちが1回分の検証（数分）に及ぶことはある。区切りに着いたら、supervisorはDBの接続をトランザクションの無い状態で閉じ（SQLiteのfcntlのlockは同じpidのexecを越えて残り、新しい接続を混乱させるので、DBを含むfile descriptorはclose-on-execにする）、新しいバイナリを自分のpidのまま`exec`する。引数は起動時の`supervise`の引数（`--parallel`・`--log-dir`・`--cmux`・`--claude`・`--plugin-dir`など）をそのまま渡し、自分のtokenを足す。
   - **lease は保たれる。** execしたプロセスは同じpid・同じtokenのsupervisorで、`supervisors`の登録も`run_leases`の行も書き換えずに引き継ぎ、heartbeatをlease TTL（30秒）より短い間に再開する。DBから見れば同じsupervisorが動き続けているので、leaseはstaleにならず、[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)の引き継ぎ（adopt）も`recover`も起きない。execしたプロセスは、自分のtokenのleaseを持つrunのslotを、adoptと同じ手順でDBとrunのファイルから組み立て直すが、それはadoptではなく、`run_adopted`は書かない。execに30秒以上かかってleaseがstaleと見なされたときは、ADR-0012（またはそれを置き換える統合ADR）の規則がそのまま効く: 別のsupervisorがadoptしていれば、execしたプロセスは「leaseを失ったsupervisor」としてそのrunに触らない。引き継ぎは、leaseの持ち主を決める規則に新しい経路を足さない。
   - **workspaceとmodeは変わらない。** in-cmux modeのsupervisorは同じcmux workspaceの同じプロセスのままで、workspaceを開き直さない。launchd modeのsupervisorは同じpidなので、launchdの`KeepAlive`は再起動を起こさない。`mode` / `workspace_id`もそのまま残る。
   - **wrapperは変えない。** 走っているrunのwrapperはclaim時のバイナリのコピー（`runs/<id>/runner`）で動き続ける。互換の範囲のschemaならそのまま動く（決定7）。
11. **バイナリの差し替えはrenameで行い、前のバイナリを残す。** 新しいバイナリは、置き換える先と同じディレクトリの一時ファイルに書き、実行権を付けてから`rename`で置き換える。動いているプロセスは前のファイルのinodeを使い続けるので、上書きのcpでプロセスが殺されることはない。置き換える前のバイナリは`<名前>.previous`（`~/.local/bin/dagq.previous`）として残し、rollbackに使う。1世代だけ残す。
12. **入れ替えの前に確かめる**（ADR-0014の「起動できないと分かる条件は止める前に確かめる」を引き継ぐ）。差し替えの前に、(a) 新しいバイナリの`--version`が期待するbuild識別子を出すこと、(b) 使い捨てのrepositoryとqueueで`init`から`supervise --once`までが起動できること、を確かめる。どちらかが通らなければ、何も差し替えず、supervisorにも何も要求しない。launchd modeで、plistの`ProgramArguments`の実行ファイルと違うpathのバイナリに入れ替えることはしない（execはplistの定義を変えないので、次にlaunchdが起動したときに前のpathに戻る。pathを変えるときは`down --wait`と`up`で行う）。
13. **入れ替えの後に確かめ、失敗すれば戻す。** execの前に、supervisorは前のバイナリ（`.previous`）で動く短命の見張りを、自分のsessionとprocess groupから切り離して起動する（execにも、止められるsupervisorにも巻き込まれないため）。見張りは、execした新しいsupervisorが期限（既定60秒）の内に、同じtokenの登録に新しいbuild識別子を書き、heartbeatを続け、ループの1周を終えることを待つ。期限を過ぎたら、見張りは`.previous`をrenameで元の名前に戻し、同じtokenのプロセスが生きていれば止め、前のバイナリでsupervisorを起動し直し（launchd modeは`KeepAlive`に任せ、in-cmux modeは記録したworkspaceを閉じてから`up --in-cmux`と同じ経路で開き直す）、inbox宛てのattention（`update_failed`、前後のbuild識別子と見張りの観測）を立てる。起動し直したsupervisorは新しいtokenなので、走っているrunは[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)（またはそれを置き換える統合ADR）のadoptで引き継がれる。互換のmigrationを適用した後でも、前のバイナリは下限以上なので同じDBで動ける（決定7）。見張りと戻しは、`.previous`が入れ替え前に動いていたbuild識別子と一致するときだけ行う。一致しない（人が`install`を経ずにファイルを置き換えた、など）ときは戻さず、`update_failed`で知らせるだけにする。

### 人の入口と自動更新

14. **`dagq install`。** 人がバイナリを入れ替える入口。main の checkout（queueが束縛されたrepositoryのroot）、または`--path`で指定したcheckoutから`cargo build --release --locked`でビルドし、決定12の確認、互換のmigration（決定5）、差し替え（決定11）、このqueueのliveなsupervisorへの入れ替えの要求（決定10）、決定13の見張りまでを1コマンドで行う。`--rollback`は`.previous`に戻し、同じ手順で引き継がせる。ビルドに非互換のmigrationが含まれるときは、`--allow-breaking`を付けたときだけ進み、そのときはADR-0014と同じdrainを行う: claimを止めさせ、持っているrunの完了を待ち、supervisorが登録を消すのを待ってから、DBを退避して（決定9）migrateし、差し替えて`up`と同じ経路で起動する（execの引き継ぎではない。modeはADR-0014の決定4のとおり、この`install`に指定されたmode（`--in-cmux`の有無）にする。drainに上限は置かない）。非互換のmigrationを適用した後の`--rollback`は、前のバイナリがDBを開けないので拒否し、退避したDBのpathを案内する。
15. **`up`の入れ替えも引き継ぎにする**（ADR-0014の決定2を引き継いで変える）。`up`はliveな登録（PIDが生きていてheartbeatが30秒以内）のbuild識別子が自分と1つでも違えば、reuseせずに入れ替える。入れ替えは、互換の範囲ならmigrate（決定5）してから、liveな登録のすべてに自分のpathのバイナリへの入れ替えを要求し（決定10。登録が複数あればそれぞれが引き継ぐ。ADR-0014の「全部drainして1つ立て直す」は、drainが無くなったので引き継がない）、見張り（決定13）の結果を待って`restarted`（`version`・`previous_version`・`replaced`と、引き継ぎかdrainか）を返す。要求はqueue DBに書き、supervisorは次の区切りで拾う（signalはlaunchd modeとin-cmux modeで届け方が違い、区切りまで待たせる必要もあるため）。非互換のmigrationを含むときは、`up`は入れ替えずに`dagq install --allow-breaking`を案内するerrorで止まる。build識別子が同じなら従来どおり`reused`。`up --no-wait`は、drainを伴う入れ替えが無くなったので、走っているrunの有無を見ずに引き継ぎを行う（非互換のときは上のとおり止まる）。ADR-0014の`--no-wait`のdrainの上限（`startup_timeout`）は、引き継ぎでは見張りの期限（決定13）が代わる。引き継ぎはプロセスを保つのでmodeは変わらず、ADR-0014の決定4（起動し直すmodeは`up`の指定に従う）はdrainで起動し直す経路（決定14）にだけ残る。modeを変えたいときは`down --wait`と`up`で行う。`up`の経路は人がファイルを置き換えた後に打たれるので、決定11のrenameと`.previous`は`up`自身では作られない。上書きの`cp`の危険（Context 4）を避け、戻しを効かせるには`dagq install`を使う。
16. **判定の対象はliveな登録だけ**（ADR-0014のConsequencesを決定として引き継ぐ）。PIDが生きていてheartbeatの止まった「生きているが黙っている」supervisorには、`up`も`install`も自動更新も入れ替えを要求せず、reuseもpruneもkillもしない。要求を拾う区切りに着けないからである。`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直す。
17. **自動更新は`up --auto-update`で有効にする。** `up --auto-update`は、起動するか引き継がせるsupervisorの登録に自動更新の設定を書く（`mode`と同じく`up`が書き、登録と寿命を共にし、execの引き継ぎでは残る）。付けない`up`は設定を消す。有効なsupervisorは次のように動く。
    - **きっかけ**: このqueueのmainが進み（supervisorの着地でも、人が手で打った`integrate`でもよい。supervisorは各passでmainのheadを最後にビルドしたcommitと比べる）、その間のcommitがruntimeを形作るパス（`src/`・`migrations/`・`Cargo.toml`・`Cargo.lock`・`build.rs`）を変えていること。`build.rs`はbuild識別子を埋め込むので含める。
    - **ビルド**: queueのdirの下の専用のcheckout（`<queue dir>/update/checkout`。着地したcommitを指すdetachedのworktree）と専用のtarget（`<queue dir>/update/target`）で、`cargo build --release --locked`をbackgroundの子プロセスとして走らせる。人のmainのcheckoutと`target/`は使わない（ビルド中に人がmainを触っても、並行するrunのビルドとtestとも混ざらない）。監視のループは止めない。ビルド中に次の着地があれば、今のビルドが終わってから最新のcommitを1回だけビルドする（着地ごとに積まない）。
    - **確認と入れ替え**: ビルドが通れば、決定12の確認、互換のmigration、差し替え、決定10の引き継ぎ、決定13の見張りを`install`と同じ手順で行う。ビルド・確認・見張りのどれかが失敗すれば、差し替えないか前のバイナリに戻し、inbox宛てのattention（`update_failed`）を立てて、次のきっかけまで自動更新を続ける。
    - **非互換のmigration**: ビルドに非互換のmigrationが含まれるときは、自動では入れ替えず、inbox宛てのask（`approve_update`: `install`（`dagq install --allow-breaking`でdrainして入れ替える）/ `skip`）にする。
    - 同じバイナリを使う別のrepositoryのqueueのsupervisorは、差し替えの後も前のバイナリのinodeで動き続け、それぞれの`up`・`install`・自動更新で入れ替わる。そのqueueのDBが新しいバイナリより古ければ、新しいバイナリでのCLIは決定5のerrorで`dagq migrate`を案内する。

### 開発中のバイナリで本番のqueueを開く規則

18. **AGENTS.mdの「開発中のバイナリ（`target/`など）で本番のqueueを開かない」規則は、この決定の実装が入った固定バイナリに入れ替えた後に、次の形に緩める。** 開発中のバイナリで本番のqueueを読むこと（`status`・`show`・`list`・`graph`・`stats`など、状態を変えないコマンド）は許す。そのためにこれらのコマンドはDBを読み取り専用で開き、openでpragmaもイベントも書かない。開いただけではmigrateせず（決定5）、DBが開発中のバイナリより新しくても互換の範囲なら読め、下限より古いバイナリは拒否され、開発中のバイナリのschemaの方が新しければそのバイナリがerrorで止まるだけなので、DBも走っているsupervisorとrunも壊れない。状態を変えるコマンド、`migrate`、`up` / `down` / `install`は、従来どおり固定バイナリで打つ。開発中のバイナリはcommitされていない変更や未着地のmigrationを含みうる、その状態遷移とschemaを本番に持ち込まないためである（`migrate`は決定8で走っているものがあれば非互換を適用しないが、互換のmigrationでも未着地のものが本番に入ると、着地したmigrationと番号が食い違う）。実装が入るまでは、今の固定バイナリが開いただけでmigrateするので、AGENTS.mdの規則はそのまま守る。

## Alternatives

- **本番と開発で別々の仕組みを持つ**: リリースの利用者には今の`up`（versionが違えばdrain）を残し、ドッグフーディングにだけ引き継ぎや自動更新を足す、あるいは開発版は別の名前のバイナリや別のqueueで動かす。仕組みが2つあると、ドッグフーディングで確かめたものがリリースの利用者の経路と違い、ドッグフーディングの意味が薄れる。build識別子の全体で判定すれば、利用者にはリリースごとにしか変わらないので、仕組みを1つにしても利用者の側で入れ替えが増えない。採らなかった。
- **versionをビルドごとに上げる**: runtimeを変えるtaskごとに`Cargo.toml`のpatch versionを上げれば、今の`CARGO_PKG_VERSION`の判定のまま入れ替わる。並行するrunが同じversionを上げてrebaseで衝突し、versionの数字がリリースと関係なく進み、crates.ioとGitHub Releaseのversionとの対応が崩れる。上げ忘れれば今と同じ穴が残る。ADR-0014がbuild hashを退けた理由（versionの意味が2つになる）は、SemVerのpre-releaseとbuild metadataで`X.Y.Z-dev+<commit>`とすればversionの規則の中に収まるので、今は当たらない。
- **drainを続ける**: 待つ時間はrunの長さそのもので、runtimeを変えるcommitが着地するたびに数時間claimが止まる。自動更新と両立しない。走っているsessionはwrapperの下で動いていて、supervisorが替わってもleaseとrunのファイルから続けられる（ADR-0012が示した）ので、sessionの終わりを待つ理由は、非互換のmigrationでwrapperがDBを開けなくなる場合にしかない。そこだけに残した（決定8・14）。
- **自動更新をしない**: 人が`dagq install`を打つ入口だけにする。着地のたびに人が打つか、古いバイナリのsupervisorが新しいcommitのtaskを捌き続けるかのどちらかになり、ドッグフーディングが着地したruntimeを使うまでの遅れが人の手に依存する。ユーザーは最初から入れると決めた（2026-09-25）。
- **`dagq.toml`で自動更新を設定する**: この repository は`dagq.toml`を置かないと決めている（2026-09-23のユーザー決定、AGENTS.md）。また`dagq.toml`はrunのenvを渡すためのもので（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定3）、main checkoutの作業ファイルから読まれるので、着地で変わった値がいつ効くかがsupervisorの起動と結び付かない。`up`のフラグなら、そのsupervisorの登録と寿命を共にし、`status`で見える。採らなかった。
- **supervisor自身にバイナリのmtimeやversionを見張らせる**（ADR-0014で退けた案）: ファイルの変化を見て自分で入れ替えると、半端に書かれたファイルや人が意図しない差し替えにも反応する。ADR-0014はin-cmux modeに再起動の主体がいないことも理由にしたが、同じpidのままのexecならin-cmux modeでも再起動の主体は要らない。そこで、入れ替えの主体はsupervisorにしつつ、きっかけは明示したもの（`install` / `up`の要求、自動更新の着地）に限った。
- **明示的な`up --restart`**（ADR-0014で退けた案）: 既定をreuseにすると、バイナリを置き換えたのに古いsupervisorが走り続けるという事故の形が残る。既定を入れ替えにする理由は変わらない。
- **`binary_version`をqueue dirのsidecar fileに置く**（ADR-0014で退けた案）: build識別子は登録されたプロセスの性質なので、登録行と寿命を共にする。fileはプロセスが死んだ後も残り、独自のstale判定を要する。
- **schema versionで判定する**（ADR-0014で退けた案）: migrationを伴わない修正版のバイナリを見分けられない。
- **開いたときに互換のmigrationだけは自動で適用する**: 互換のmigrationは古いバイナリを壊さないが、開発中のバイナリが未着地のmigrationを本番に入れる経路が残り、番号が着地したmigrationと食い違う（決定18）。migrateの時点を入れ替えの手順に揃えれば、どのバイナリがいつschemaを上げたかがコマンドと結び付く。採らなかった。
- **headless jobの終わりも待ってからexecする**: reviewやtriageのjobは数分かかることがあり、入れ替えの待ちがjobの長さに引きずられる。jobはrunの状態から何度でもやり直せるので、止めてやり直す方を採った。

## Consequences

- 固定バイナリの更新に待ちが無くなる。互換の範囲のビルドなら、`install`・`up`・自動更新のどれでも、supervisorは区切りに着いた時点でexecし、走っているsessionはそのまま続く。drainが残るのは非互換のmigrationを含むビルドだけで、それは人が`install --allow-breaking`で選ぶ。
- migrationを書くtaskは、互換か非互換かの宣言も書く。宣言を誤って非互換の変更を互換と書くと、古いバイナリが知らない値を読んで失敗しうる。宣言の妥当性（互換と書いたmigrationが表と列の追加だけか）を検査するtestを置く。
- 移行は一度だけ非互換になる。決定7の下限を入れるmigrationは非互換なので、この実装が入った最初のバイナリへの入れ替えは、旧バイナリで`down --wait`（drain）→ バイナリの置き換え → 新バイナリで`dagq migrate` → `up`の順で行う（新バイナリの`up`は非互換のmigrationを含む入れ替えを拒むので、旧supervisorが動いたまま新バイナリの`up`を打っても止まる）。それ以後の互換のmigrationから、待たない入れ替えが効く。
- `dagq --version`とbuild識別子の埋め込みのために`build.rs`を足す。crates.ioのsourceからのリリースのビルドは`X.Y.Z`で、gitに依らない。
- 開いただけでmigrateしないので、`install`を経ずにバイナリだけを置き換えた後、DBが古ければCLIは`dagq migrate`を案内して止まる。1コマンドで直るが、今までは黙って直っていた。
- 自動更新は`<queue dir>/update/`にcheckoutとtargetを持つ。targetはrelease buildの分だけディスクを使う。checkoutのworktreeは`git worktree list`に出る。
- `.previous`は1世代しか残らないので、2回続けて失敗した入れ替えの後の`--rollback`は、最後に動いていたバイナリにしか戻れない。
- [ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)の`binary_replaced`は、execしたプロセスの起動でも前後のbuild識別子とともに記録され、その入れ替えは`up`のdrainによるものではない。
- 見張りのための短命のプロセスが入れ替えのたびに1つ立つ。見張りが自分で失敗した（期限の判定の前に死んだ）ときは戻しも知らせも起きないので、新しいsupervisorが動かなければ、従来どおりinboxの`supervisor_stopped`が人に知らせる。
- AGENTS.md・dagq-recover skill・release skill・[supervisor-lifecycle](../design/supervisor-lifecycle.md)・[persistence](../design/persistence.md)・README.mdの更新手順と記述は、この決定を実装するtaskで書き直す。それまでの記述は今の実装（ADR-0014の仕組み）を説明している。
