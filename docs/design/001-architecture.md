# 設計 #001: モジュール構成・マルチエージェント・通知

| 項目 | 内容 |
|---|---|
| 作成日 | 2026-11-08 |
| ステータス | ドラフト（提案） |
| 前提 | Linux カーネル上の単一ユーザーランド。実装は Rust。macOS / Windows / Android / iOS にも展開する。**プラグインも Rust に限定する**（2026-11-08 決定） |
| 関連 | [調査 #001](../research/001-ai-os-optimization-perspectives.md) / [調査 #002](../research/002-file-management.md) / [図解](../diagrams/index.html) |

---

## 1. 設計方針

1. **ポートとアダプタ（ヘキサゴナル）構成にする。**
   - 中核は trait（ポート）だけに依存させる。
   - OS や外部サービスとのつなぎ込み（アダプタ）は、すべて付け替えられるようにする。
2. **中核を小さく保つ。**
   - 安全性を担う部分は中核に置き、プラグインからは迂回できないようにする。対象はブローカー、ポリシー判定、ラベル伝播、監査ログ。
   - プラグインは必ずブローカー経由で動く。
3. **抽象化しても、保証の差は隠さない。**
   - アダプタは「何をどこまで守れるか」を自己申告する。
   - ポリシーは、保証が足りない環境ではそのタスクを拒否できるようにする。
   - 例：macOS ネイティブの隔離は Linux より弱い、という差を中核が知った上で判断する。
4. **組み立ては設定で行う。** どのアダプタを使うかは設定ファイル（TOML）で選ぶ。中核のコードは変えない。

---

## 2. 付け替えの3つの方式

| 方式 | 向いているもの | 長所 | 短所 |
|---|---|---|---|
| **A. コンパイル時**（trait + Cargo feature） | OS アダプタ（サンドボックス、作業領域、ファイル監視、秘密情報） | 速い。型で安全性を確かめられる | 差し替えには再ビルドが必要 |
| **B. 別プロセス**（Rust 製バイナリ、Cap'n Proto RPC） | モデルプロバイダ、ツール、エージェント実装、通知チャネル | プラグインが落ちても本体は止まらない。プラグイン自体もサンドボックスに入れられる。権限をケイパビリティとして渡せる | IPC の手間がある |
| **C. WASM コンポーネント**（wasmtime + WASI 0.2。公式に対応する記述言語は Rust） | ポリシーの追加ルール、ラベル分類器、イベントのフィルタ | 隔離され、どの OS でも同じ動きをし、配布しやすい | 使える機能が限られる |

**プラグインの言語は Rust に限定します**（2026-11-08 決定）。理由は3つです。
- 実行環境（Python、Node など）を OS に持ち込まずに済み、攻撃される面とサプライチェーンのリスクが小さくなる。
- 型と所有権の検査により、プラグインのメモリ安全性を保証できる。
- 中核から部品まで同じ言語なので、Cap'n Proto の弱点（他言語への対応）が問題にならない。

### 2.1 IPC：Cap'n Proto を中心にする

| 境界 | 方式 |
|---|---|
| 中核 ⇔ OS アダプタ・サンドボックス内のタスク・プラグイン | **Cap'n Proto RPC**（Unix ソケット／名前付きパイプ。接続元のプロセスを OS に確認させる） |
| 大きなデータ（ファイルの中身、埋め込みベクトル） | 複製せずにハンドルを渡す（Linux なら memfd と SCM_RIGHTS） |
| MCP サーバ（外部） | **ゲートウェイ**で JSON-RPC ⇔ ブローカーの呼び出しに変換 |
| A2A・ACP（外部エージェント） | 同じくゲートウェイで変換 |

Cap'n Proto の弱点は、次のように補います。
- **権限が永続しない**：Cap'n Proto のケイパビリティは接続中しか有効でない。そこで、監督（`otas-supervisor`）が、再起動をまたいで有効な「永続トークン ⇔ ケイパビリティ」の対応表を管理する（SturdyRef に相当するものを自前で持つ）。
- **非同期処理との相性**：`capnp-rpc` は1スレッドで動く前提なので、接続ごとに専用スレッド（tokio の `LocalSet`）で動かし、中核とはチャネルでつなぐ。
- **周辺ツールの少なさ**：監査ログとは別に、RPC の呼び出しを人間が読める形で書き出すデバッグ用ツールを用意する。

**MCP サーバへの影響**：既存の MCP サーバの多くは TypeScript や Python 製です。OtaS ではこれらをローカルのプラグインとしては動かしません。使い方は、リモートの MCP サーバとしてゲートウェイ経由で使うか、Rust 版を用意するかのどちらかです。

OtaS 独自のプロトコルは、標準でカバーできない部分（イベント、承認、ラベル、ケイパビリティ）だけに絞ります。

---

## 3. クレート構成（Cargo ワークスペース）

```
crates/
  otas-types        ID・ラベル・権限（ケイパビリティ）・イベントの型。依存なし。全クレートの共通語彙
  otas-ports        すべての trait 定義（下の §4）。otas-types だけに依存

  # 中核（安全性を担う。プラグインから迂回できない）
  otas-supervisor   タスクのライフサイクル、委任ツリー、予算、スケジューリング
  otas-broker       すべての呼び出しの仲介。ポリシー判定 → 実行 → 監査
  otas-ifc          ラベルの伝播と「Rule of Two」の判定
  otas-audit        追記専用・ハッシュチェーンの監査ログ
  otas-bus          イベントバス（通知・トリガー・エージェント間メッセージ）

  # 標準実装（どれも付け替え可能）
  otas-policy-cedar     ポリシーエンジン（Cedar。Rust 製で検証済みの仕様がある）
  otas-workspace-git3   3者比較によるコミット判定
  otas-index            意味層の索引（全文検索・ベクトル）

  # プロトコル
  otas-ipc          Cap'n Proto のスキーマと RPC 層（Unix socket / named pipe、接続元の確認、ハンドル渡し）
  otas-gateway      外部プロトコル（MCP・A2A・ACP）とブローカーの呼び出しを相互変換
  otas-mcp          MCP クライアント・サーバ
  otas-a2a          A2A（外部エージェント）
  otas-acp          ACP（Claude Code・Codex などのコーディングエージェント）

  # OS アダプタ（方式 A。cfg と feature で選ぶ）
  otas-adapter-linux     namespaces・Landlock・seccomp・cgroups・Btrfs・OverlayFS・fanotify
  otas-adapter-macos     Seatbelt・APFS clonefile・FSEvents・Keychain
  otas-adapter-windows   AppContainer・Job Object・ReFS ブロッククローン・Credential Manager
  otas-adapter-vm        どの OS でも、Linux VM 内の otasd に処理を任せる

  # 組み立て
  otasd             デーモン。設定を読んでアダプタとプラグインをつなぐ（組み立てはここだけで行う）
  otas-cli          CLI
  otas-ffi          UniFFI による Swift / Kotlin 向けバインディング（iOS・Android の窓口アプリ用）

plugins/            方式 B・C のプラグイン（別リポジトリにしてもよい）
  model-anthropic / model-openai / model-llamacpp / model-ollama
  notify-desktop / notify-push / notify-slack
  agent-builtin
```

**依存の向き**：アダプタ → ports → types。中核 → ports → types。中核とアダプタは互いを知りません。両者をつなぐのは `otasd` だけです。

---

## 4. 主な trait（ポート）

コードはイメージです。dyn で差し替えられるように、非同期メソッドは `async_trait` か、Box で包んだ Future を返す形にします。

```rust
/// アダプタが自己申告する保証レベル。ポリシーが参照する
pub struct Guarantees {
    pub fs_isolation: Level,      // Strong / Weak / None
    pub net_isolation: Level,
    pub syscall_filter: Level,
    pub snapshot: Level,
}

pub trait Sandbox: Send + Sync {
    fn guarantees(&self) -> Guarantees;
    async fn spawn(&self, spec: SandboxSpec) -> Result<SandboxHandle>; // 見せるファイル・通信先・資源上限
    async fn checkpoint(&self, h: &SandboxHandle) -> Result<CheckpointId>;
    async fn kill(&self, h: &SandboxHandle) -> Result<()>;
}

pub trait Workspace: Send + Sync {
    async fn begin(&self, base: &DataRef, view: &ViewSpec) -> Result<WorkspaceId>; // base を固定し上書き層を作る
    async fn diff(&self, ws: &WorkspaceId) -> Result<ChangeSet>;
    async fn commit(&self, ws: &WorkspaceId) -> Result<CommitOutcome>;           // Applied(version) / Conflict(..)
    async fn discard(&self, ws: &WorkspaceId) -> Result<()>;
    async fn rollback(&self, to: VersionId) -> Result<()>;
}

pub trait ModelProvider: Send + Sync {
    fn capabilities(&self) -> ModelCaps;           // コンテキスト長・ツール呼び出し・ローカルかどうか
    async fn infer(&self, req: InferRequest) -> Result<InferStream>;
}

pub trait AgentRuntime: Send + Sync {
    /// エージェントのループそのもの。内蔵実装と、ACP・A2A の外部エージェントを同じ形で扱う
    async fn run(&self, task: TaskContext) -> Result<TaskOutcome>;
}

pub trait ToolProvider: Send + Sync {
    async fn list(&self) -> Result<Vec<ToolSpec>>;
    async fn call(&self, call: ToolCall) -> Result<ToolResult>;    // 呼び出し元は常にブローカー
}

pub trait PolicyEngine: Send + Sync {
    fn decide(&self, req: &AccessRequest, labels: &LabelSet, env: &Guarantees) -> Decision; // Allow / Deny / AskHuman
}

pub trait EventBus: Send + Sync {
    async fn publish(&self, ev: Event) -> Result<EventId>;
    async fn subscribe(&self, filter: EventFilter) -> Result<Subscription>;
}

pub trait Notifier: Send + Sync {
    fn channel(&self) -> ChannelInfo;               // ローカルか外部か。外部なら送信扱い（ラベル判定の対象）
    async fn notify(&self, n: Notification) -> Result<DeliveryId>;
}

pub trait AuditSink: Send + Sync { async fn append(&self, rec: AuditRecord) -> Result<RecordHash>; }
pub trait SecretStore: Send + Sync { async fn inject(&self, req: &OutboundRequest) -> Result<OutboundRequest>; }
pub trait FsWatch: Send + Sync { async fn watch(&self, scope: &DataRef) -> Result<EventStream>; }
```

**呼び出しの流れ**：エージェントは `ToolProvider` や `ModelProvider` を直接呼びません。必ず次の順で処理されます。

1. ブローカーが呼び出しを受ける。
2. `otas-ifc` がラベルを確認する。
3. `PolicyEngine` が判定する。
4. 許可されたら実行する。
5. `AuditSink` に記録する。

アダプタやプラグインを差し替えても、この順序は変わりません。

---

## 5. マルチエージェント対応

**対応できます。** タスクの委任ツリー（図1の「委任」）を土台にします。

### 5.1 仕組み

| 要素 | 内容 |
|---|---|
| 委任ツリー | タスクは子タスクを作れる。子に渡せる権限と予算は、親の範囲内に限られる（縮小方向のみ）。Erlang の監督ツリーのように、親が子の失敗を検知して再起動または中止する |
| エージェント間通信 | 直接は話さず、すべてイベントバス経由にする。**メッセージにもラベルが付き**、受け取った側にラベルが伝播する。他のエージェントを経由して制限をすり抜けることはできない |
| 共有作業 | 同じファイルを触る場合は、それぞれの作業領域で作業し、コミット時に3者比較する（調査 #002 §4）。競合しやすい資源にはリース（期限付きの排他）を使う |
| 外部エージェント | A2A（リモート）と ACP（Claude Code・Codex など）のエージェントも、`AgentRuntime` を通して同じ委任ツリーに入る。外部エージェントもサンドボックスとブローカーの下で動く |
| 資源の配分 | 同時に動くタスク数の上限、推論の優先度（対話中のタスク > バックグラウンド）、親から子への予算の分配 |

### 5.2 標準で用意するパターン

いずれも `otas-supervisor` 上の部品として提供し、組み合わせて使えるようにします。

- **指揮役と作業役**（orchestrator–worker）：分解して並列に投げ、結果をまとめる。
- **パイプライン**：調査 → 下書き → レビューの順に渡す。
- **レビュー役**：別のエージェントが成果物を検証してから、人間のレビューに回す。
- **計画役と隔離された実行役**（CaMeL 型）：信頼できない入力は、権限を持たない隔離エージェントだけが読む。計画役はその「値」しか受け取らない。これは、ラベルの広がりすぎ（調査 #002 §7）への対策も兼ねる。

---

## 6. OS 内通知への対応

**対応できます。** 通知はイベントバス（`otas-bus`）の上に作ります。通知には2つの向きがあります。

### 6.1 エージェントへのイベント（トリガー）

| イベントの発生源 | 例 |
|---|---|
| 時刻 | 毎朝 8 時、締切の 1 日前 |
| ファイル | `/data/projects/a` の変更（`FsWatch`） |
| データ | メールの着信、予定の追加 |
| タスク | 他のタスクの完了・失敗・コミット |
| 外部 | Webhook（ラベル：信頼できない） |
| システム | 電源接続、ネットワークの変化、ストレージ残量 |

- イベントは**永続化**します（追記ログ）。止まっているエージェントでも、購読条件に合えば監督がタスクを起動します。
- 配送は「少なくとも1回」とし、冪等キーで重複を排除します。
- **イベントにもラベルを付けます。** 外部から来たイベントで起動したタスクは、最初から「信頼できない」ラベルを持ちます。

### 6.2 人間への通知

| 種類 | 内容 |
|---|---|
| 承認の依頼 | 高リスクの操作、アウトボックスの保留（図3） |
| レビューの依頼 | タスクの完了と差分 |
| 報告 | 完了、失敗、予算超過 |

設計上の要点：
- **通知チャネルはプラグイン**にします（`Notifier`）。Linux デスクトップは freedesktop 通知（D-Bus、`zbus` クレート）、スマホへのプッシュは iOS・Android の窓口アプリ経由、Slack やメールは外部プラグインです。
- **外部チャネルへの通知は「外部送信」として扱います。** 通知の本文も情報の流出経路になるので、ブローカーでラベルを判定します。私的データを含む通知を Slack に送る、といった操作は止めます。
- **承認は、通知を受け取った先で直接確定させません。** 通知に載せた署名付きの承認リンク、または OtaS 本体の画面で確定します。偽の通知による承認を防ぐためです。
- **承認疲れを防ぎます。** まとめて通知する、優先度で分ける、通知しない時間帯を設ける、計画単位でまとめて承認できるようにする（調査 #001 §3.7）。

---

## 7. 設定例（組み立て）

```toml
[platform]
sandbox   = "linux"          # linux / macos / windows / vm
workspace = "btrfs-overlay"

[policy]
engine = "cedar"
min_guarantees = { fs_isolation = "strong" }   # これを満たさない環境では高リスクタスクを拒否

[[models]]
id = "local-small"
plugin = "model-llamacpp"
[[models]]
id = "cloud-large"
plugin = "model-anthropic"
allowed_labels = ["none"]                      # 私的データはクラウドに送らない

[[agents]]
id = "builtin"
plugin = "agent-builtin"
[[agents]]
id = "coder"
protocol = "acp"                                # 外部のコーディングエージェント

[[notifiers]]
plugin = "notify-desktop"
[[notifiers]]
plugin = "notify-slack"
external = true                                 # 外部送信としてラベル判定の対象にする
```

---

## 8. 決定事項（2026-11-08）

| 項目 | 決定 |
|---|---|
| カーネル | Linux |
| 実装言語 | Rust（プラグインも Rust に限定） |
| IPC | Cap'n Proto 中心。MCP・A2A・ACP はゲートウェイで変換 |
| ライセンス | Apache-2.0 |
| MVP の範囲 | **中核ライブラリのみ**：`otas-types`・`otas-ports`・`otas-ifc`・`otas-policy`・`otas-broker`・`otas-audit` をライブラリとして実装し、テストで動作を確かめる。IPC・OS アダプタ・デーモンは次の段階 |
| 対応するモデル | Claude API・ローカルモデル（llama.cpp / Ollama）・OpenAI 互換 API。MVP では `ModelProvider` trait とテスト用の偽実装まで。実際のプロバイダは次の段階でプラグインとして作る |

## 9. 未決定事項

- **WASM プラグインの範囲**：ポリシーとフィルタだけに限るか、ツールにも広げるか。
- **タスク内で実行できるもの**：型付きツールを基本とし、シェルは POSIX sh と許可したコマンドだけ、Python などは必要なタスクにだけ追加する案（未決定）。
- **プラグインの配布と信頼**：署名、出所の検証、権限の宣言（マニフェスト）をどうするか。
- **バスの永続化先**：SQLite にするか、独自の追記ログにするか。

---

## 10. 実装メモ：Linux サンドボックス（`otas-adapter-linux`）

**特権を使わない構成**にしました。権限をすべて外したコンテナの中でも動きます。

親は起動役 `otas-sandbox-exec` を起動します。起動役は自分自身に次の制限をかけてから、対象のプログラムを exec します。

1. 親が死んだら道連れにする（`PR_SET_PDEATHSIG`）
2. 資源の上限（`setrlimit`。コアダンプは常に無効）
3. **Landlock**（ABI v4、Linux 6.7 以降）：許可したパス以外に触れない。TCP の bind と connect をすべて拒否する
4. **seccomp**：危険なシステムコールを拒否する
   - 対象：名前空間の作成、ptrace、マウント、ファイルハンドル、カーネルモジュール、io_uring、鍵の保管庫、ソケットの作成など
   - `clone3` は ENOSYS を返す（引数が構造体で検査できないため。libc は `clone` に切り替える）
5. 環境変数を空にし、指定したものだけを渡す

ブローカーとの IPC の通り道は、子の **stdin** として渡します（MCP の stdio と同じ考え方）。

**保証の申告**

| 項目 | 申告 | 理由 |
|---|---|---|
| ファイルの隔離 | Strong | Landlock がカーネルで強制する |
| ネットワークの隔離 | Strong | Landlock の TCP 制限と、seccomp によるソケット作成の拒否 |
| システムコールの制限 | **Weak** | 危険なものを列挙して拒否する方式で、許可リスト方式ではない |
| スナップショット | Absent | 作業領域のアダプタは未実装 |

**既知の制限**

- Landlock ABI v4 が使えない環境では起動を拒否する（制限なしでは動かさない）。
- ~~許可していないパスは「アクセスできない」だけ~~：名前空間の方式（§15）で、許可していないパスは「存在しない」ようにできる。
- Linux 6.12 未満では、Landlock で抽象 Unix ソケットへの接続を制限できない。そこで、seccomp でソケットの作成そのものを拒否している。
- テスト環境（Docker）は Docker 自身の seccomp も効いているため、「名前空間を作れない」テストは OtaS と Docker のどちらで止まったかを区別できていない。ネットワークのテストは「接続を断られた」のではなく「ソケットを作れない」ことを確かめている。

---

## 11. 実装メモ：デーモン（`otasd`）

- **窓口は3種類**：
  - `TaskSession`：タスク専用。stdin で渡す
  - `Approver`：承認用
  - `Console`：人間向け。依頼・一覧・承認の窓口の取得ができる
- **`Console` の公開方法**：`state_dir/console.sock` で公開します。ソケットは 0600、ディレクトリは 0700 にし、接続元のユーザーIDがデーモンと同じでなければ窓口を渡しません。
- **見えるファイルは権限から決める**：依頼の `grants` の `read` / `write` から、サンドボックスで見えるファイルを自動で決めます。権限と見え方の基準を1つにするため、ファイルの規則を別に書く手段は用意していません。
- **ログ**：エージェントの標準出力と標準エラー出力は `state_dir/tasks/<id>/` に保存します。
- **保証の要求**：外部に作用する操作に必要な保証の既定値は、Linux サンドボックスの現状に合わせています（ファイル・ネットワークは Strong、システムコールは Weak、スナップショットはなし）。

**既知の制限**

- ~~ファイルを直接読んだ場合のラベル~~：作業領域（§12）で解消しました。作業領域に見せたデータのラベルは、タスクの開始時にタスクへ取り込まれます。
- ~~状態が残らない~~：状態の保存（§14）で解消しました。
- **動作確認用のツール**：`demo.*` は、設定の `[demo] tools = true` のときだけ登録します。

---

## 12. 実装メモ：作業領域（`otas-workspace`）

調査 #002 §4 の「書き込みと確定の流れ」を、**特権を使わない方式**で実装しました。Btrfs のスナップショットと OverlayFS による実装は、同じ `Workspace` trait の別実装として後から追加します（どちらもマウントなどの特権が必要で、今の開発用コンテナでは使えないため）。

**置き場所**（`state_dir/store/`）

| パス | 中身 |
|---|---|
| `live/` | 正本（論理パス `/data`） |
| `versions/<n>/` | 版。正本をハードリンクで丸ごと複製したもの |
| `work/<task>/view/` | タスクの作業領域（タスクからは `OTAS_DATA` として見える） |
| `meta/labels.json` | パスごとのラベル |

**流れ**

1. **開始**：現在の版を元に作業領域を作ります。書き込みを許す範囲は本物のコピー、読み取りだけの範囲はハードリンクにして、Landlock で書き込みを禁止します。見せた範囲のラベルは、ブローカーの `read_data` を通してタスクに取り込み、監査ログにも残します。
2. **終了**：作業領域に変更がなければ自動で片付けます。変更があれば「レビュー待ち」にします。
3. **コミット**：変更する各パスをブローカーの `write_data` で判定して記録したうえで、元にした版・作業領域・現在の正本の3つを比べます。衝突があれば何も反映しません。反映するときは「一時ファイルに書いてから置き換える」ので、ハードリンクを共有する過去の版は壊れません。書いたファイルにはタスクのラベルを付け、新しい版を作ります。
4. **破棄と巻き戻し**：破棄は作業領域を消すだけです。巻き戻しは指定した版を正本に戻し、それを新しい版として記録します。

**安全性**

- コミットでは、作業領域のファイルを中身としてコピーします。シンボリックリンクなど通常のファイル以外は拒否します。タスクが置いたリンクをデーモンがたどって、見せていないファイルを持ち出すのを防ぐためです。
- 書き込める権限（`write`）は読み取りも含みます。親タスクが `write` を持っていれば、子タスクに `read` を委任できます。

**保証の申告**

版（スナップショット）の保証は **Weak** としています。カーネルが強制するスナップショットではなく、OtaS 以外のプロセスが正本のファイルをその場で書き換えると、ハードリンクを共有する版も変わるためです。

**既知の制限**

- **作業領域の作成コスト**：コピー方式では、書き込みを許す範囲を丸ごとコピーします。OverlayFS の方式（§15）なら、書いたファイルだけがコピーされます。
- **作業領域のパス**：コピー方式では、作業領域のパスを `OTAS_DATA` で知らせます。名前空間の方式（§15）なら `/data` そのものとして見えます。
- **ラベルは巻き戻らない**：巻き戻してもラベルは戻しません。多めに付いている分には安全側だからです。
- **コミットの途中で落ちた場合**：正本が一部だけ更新された状態になりえます。版は作られないので、直前の版に巻き戻して復旧します。
- **Landlock の範囲外**：読み取りだけの範囲でも、ファイルの権限ビット（chmod）の変更は Landlock の制限対象外です。ハードリンクを共有する正本のファイルの権限ビットは変えられてしまいます（中身は変えられません）。

---

## 13. 実装メモ：モデルのアダプタ（`otas-models`）

| プロバイダ | 接続先 | 推論の影響範囲 |
|---|---|---|
| `anthropic` | Claude API（`POST /v1/messages`、`anthropic-version: 2026-10-21`）。既定のモデルは `claude-opus-5-5` | 常に外部への作用 |
| `openai_compatible` | OpenAI 互換の `POST {base_url}/chat/completions`（Ollama、llama.cpp の `llama-server`、vLLM など） | `local = true` なら内部、そうでなければ外部 |

Rust には公式の Anthropic SDK がないので、HTTP で直接呼んでいます。

**Claude API 固有の扱い**

- `stop_reason: "refusal"`（安全のための分類器による拒否）は、本文を読まずに `ModelError::Refused` として返す。
- 既定で `fallbacks: "default"`（`anthropic-beta: server-side-fallback-2026-07-01`）を付け、断られた要求をサーバ側で別のモデルに切り替えて再実行させる。設定の `fallbacks = false` で外せる。
- effort は `output_config.effort` で明示する（既定は `high`。Claude Opus 5.5 の API 側の既定は medium のため）。
- サンプリングのパラメータ（temperature など）は送らない（`2026-10-21` では 400 になるため）。

**安全性**

- API キーは設定ファイルに書かない。環境変数名（`api_key_env`）かファイル（`api_key_file`）で渡し、デーモンの中だけで使う。サンドボックスのエージェントはキーのファイルを読めない（e2e テストで確認済み）。
- `local = true` を申告できるのは、接続先がループバックアドレスの場合だけ。リモートのサーバを「ローカル」と偽ると、私的データがラベルの判定なしに外へ出てしまうため。
- 暗号化しない `http` はループバックの場合だけ許す。URL に認証情報を書くことも拒否する。
- リダイレクトはたどらない（API キーのヘッダを別のホストに送らないため）。エラーメッセージにもキーを含めない。
- 429・5xx・接続エラーは2回まで再試行する（`retry-after` に従う。上限 30 秒）。

**未対応**

- ストリーミング、ツール呼び出し（tool use）。エージェントのループ（モデルにツールを選ばせて実行する）を作るときに追加する。
- 別プロセスのプラグイン化（§2 の方式B）。今は同じプロセスの中で動く。

---

## 14. 実装メモ：状態の保存（`otas-state`）

デーモンを再起動しても、承認待ちの操作とレビュー待ちの変更を続きから扱えるようにしました。

| 保存するもの | キー | 中身 |
|---|---|---|
| ブローカー | `broker` | タスクの権限・ラベル・親子関係、承認待ちの操作（ツールの引数や推論の要求を含む） |
| 監督 | `supervisor` | タスクの状態 |

保存先は `StateStore` trait で付け替えられます。今の実装は次の2つです。
- `FileStateStore`：`state_dir/state/<キー>.json`。ディレクトリは 0700、ファイルは 0600。一時ファイルに書いて fsync してから置き換える。
- `MemoryStateStore`：テスト用。

**安全性に関わる決めごと**

- **ラベルの保存に失敗したら、その操作をエラーにする**：再起動後にラベルが消えると、私的データに触れたタスクの変更が、ラベルなしでコミットされてしまうため。
- **承認待ちは、実行する前に外して保存する**：途中で落ちても、同じ操作（送信など）を2回実行しない（多くて1回）。
- **新しいタスクの保存は失敗しても続ける**：ラベルのない新しいタスクが再起動で消えても、後の操作が「不明なタスク」として拒否されるだけで安全側に倒れるため。次に状態が変わったときに丸ごと保存し直すので、そこで回復する。

**再起動したとき**

- 動いていたタスクのエージェントは、デーモンと一緒に止まっています（`PR_SET_PDEATHSIG`）。
- 作業領域に変更が残っていればレビュー待ちに、なければ中断（`interrupted`）として扱います。
- 承認待ちの操作は、そのまま承認・却下できます。承認するときは、保存しておいた権限とラベルでポリシーを判定し直します。

---

## 15. 実装メモ：名前空間・OverlayFS・Btrfs

デーモンを root で動かせる環境向けに、より強い方式を追加しました。Landlock だけの方式（特権不要）も、そのまま選べます。

| 設定 | 方式 | 作業領域 | 版 |
|---|---|---|---|
| `[sandbox] mode = "landlock"` | Landlock と seccomp | コピー（`OTAS_DATA` の場所に見える） | `hardlink` |
| `[sandbox] mode = "namespace"` | 名前空間 + pivot_root + 一般ユーザー + Landlock + seccomp | OverlayFS（`/data` として見える） | `hardlink` か `btrfs` |

**名前空間の方式**（`otas-adapter-linux` の `namespace` モジュール）

起動役は root で動き、次の順に準備してから一般ユーザーに切り替えます。

1. マウント・ネットワーク・IPC・UTS の名前空間を新しく作る。ネットワークは空で、外に出る経路がない。
2. 空の tmpfs を新しいルートにして、許可したものだけを置く。
   - システムのパスは同じパスに読み取り専用で見せる。`/bin` のようなシンボリックリンクはリンクとして作る。
   - 作業領域は `/data` 以下に見せる（読み取り範囲は版を bind mount、書き込み範囲は OverlayFS）。
   - `/tmp` はタスク専用の tmpfs。
3. `pivot_root` で移り、元のルートを切り離す。ルートは読み取り専用にする。
4. 補助グループを空にし、`uid` / `gid` に切り替える。ケーパビリティはすべて失われる。親の終了検知（PDEATHSIG）は切り替えで解除されるので、設定し直す。
5. Landlock と seccomp を適用して exec する。

許可していないパス（`/etc/passwd`、`/home` など）は「読めない」のではなく「存在しない」ことをテストで確認しました。`/` の一覧も Landlock が拒否します。

**OverlayFS の作業領域**

- 書いたファイルだけが上の層にコピーされます。
- 差分は上の層から読みます。削除の印（whiteout：デバイス番号 0/0 のキャラクタデバイス）は削除として、下の層を隠すディレクトリ（`trusted.overlay.opaque=y`）は中身の入れ替えとして扱います。
- マウントは `redirect_dir=off,metacopy=off,index=off,xino=off` で行います。変更をすべて上の層に実体として残し、差分を正しく読むためです。
- 正本とファイルの所有者は、タスクのユーザー（`uid` / `gid`）にそろえます（デーモンは root で動く）。
- 読み取り範囲の中に、元の版に存在しない書き込み範囲を入れ子にすることはできません。マウント先を作れないためです。

**Btrfs の版**

- 正本はサブボリュームで、版は `btrfs subvolume snapshot -r` で作った読み取り専用のスナップショットです。root でも書き換えられないことを確認しました（EROFS）。そのため、保証は **Strong** と申告します。
- ハードリンクはサブボリュームをまたげないので、OverlayFS の作業領域と組み合わせて使います。
- サブボリュームをまたぐ rename もできないので、コミットの一時ファイルは正本の中に作ります。

**テスト環境**

- 特権が必要なテストは、compose の `dev-privileged`（`privileged: true`、root）でだけ動かします。
- `scripts/test-privileged.sh` は、`/tmp` に置いた Btrfs のイメージを `mount -o loop` で使います。loop デバイスはアンマウント時に自動で切り離されます。
- 普段の `dev` サービスは特権なしのままです。特権が必要なテストは、そこでは自動で飛ばされます。

**まだ対応していないこと**

- PID 名前空間（`/proc` も見せていない）
- cgroups による資源の上限（今は rlimit だけ）
- ユーザー名前空間（root を使わない名前空間の方式）

---

## 16. 実装メモ：エージェントのループ（`otas-agent`）

`otas-agent --model <ID> <依頼>` は、モデルにツールを選ばせて実行し、結果をモデルに返すことを、モデルが終わるまで繰り返します。サンドボックスの中で動き、外に作用する手段はタスク専用の窓口だけです。

- **ツールは2種類**
  - **ブローカーのツール**：権限のあるものだけを `tools()` で受け取り、モデルに見せる。名前の `.` は `__` に置き換える（プロバイダの名前の制約）。
  - **作業領域のファイル操作**：`fs__read` / `fs__write` / `fs__list`。サンドボックスの中で直接行い、変更はレビュー待ちになる。
- **モデルの呼び出しもブローカーを通る**（`inferFull`）。私的データと信頼できない入力の両方に触れた後は、クラウドのモデルへの送信が承認待ちになり、エージェントは終了コード 3 で止まる（e2e テストで確認）。
- **承認待ちのツール**：まだ実行されていないことを、ツールの結果としてモデルに伝える。
- **思考のブロック**：Claude API の応答（`provider_raw`）を次の要求の assistant にそのまま送り返す。思考のブロックの署名を変えないため。
- **経過の記録**：1行1件の JSON で標準出力に書く。

**モデルのアダプタの拡張**

- **ツールの呼び出し**：`InferRequest.tools`、`InferResponse.tool_calls` / `stop_reason`、`Message.tool_calls` / `tool_results` / `provider_raw`。
- **Claude API のストリーミング**：`stream: true` で受け取り、最後まで組み立てて返す。思考・本文・ツールの引数の差分をつなぎ、ストリームの途中のエラーや拒否も扱う。HTTP のタイムアウトは全体ではなく、データが途切れている時間で判定する。
- **OpenAI 互換 API**：function calling に対応した。
