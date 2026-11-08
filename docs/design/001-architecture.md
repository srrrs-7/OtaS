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
- 許可していないパスは「アクセスできない」だけで、名前空間で「存在しない」ようにはしていない（図5の方式は次の段階）。
- Linux 6.12 未満では、Landlock で抽象 Unix ソケットへの接続を制限できない。そこで、seccomp でソケットの作成そのものを拒否している。
- テスト環境（Docker）は Docker 自身の seccomp も効いているため、「名前空間を作れない」テストは OtaS と Docker のどちらで止まったかを区別できていない。ネットワークのテストは「接続を断られた」のではなく「ソケットを作れない」ことを確かめている。
