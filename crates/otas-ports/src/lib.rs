//! OtaS のポート（trait）定義。
//!
//! 中核（ブローカーなど）はこのクレートの trait だけに依存する。
//! ポリシー、監査ログ、ツール、モデルの実装はすべて付け替えられる。
//!
//! 現在定義しているのは、ブローカーが使う trait（ポリシー・監査ログ・ツール・モデル）と、
//! サンドボックスの trait。作業領域、イベントバス、通知などの trait は次の段階で追加する
//! （`docs/design/001-architecture.md` §4）。

mod sandbox;
mod state;
mod workspace;

pub use sandbox::{
    ExitInfo, FsAccess, FsRule, Mount, MountSource, NetPolicy, ResourceLimits, Sandbox,
    SandboxError, SandboxProcess, SandboxSpec,
};
pub use state::{StateError, StateStore};
pub use workspace::{
    Change, ChangeKind, CommitOutcome, ViewEntry, ViewSpec, Workspace, WorkspaceError,
    WorkspaceView,
};

use async_trait::async_trait;
use otas_types::{
    AccessRequest, AuditEntry, AuditRecord, Decision, Effect, GrantSet, Guarantees, LabelSet,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ポリシー
// ---------------------------------------------------------------------------

/// ポリシーの判定に使う情報。
#[derive(Debug, Clone, Copy)]
pub struct PolicyContext<'a> {
    /// 判定する要求。
    pub request: &'a AccessRequest,
    /// 要求したタスクの権限。
    pub grants: &'a GrantSet,
    /// 要求したタスクがこれまでに積み上げたラベル。
    pub labels: &'a LabelSet,
    /// 実行環境の保証。
    pub guarantees: &'a Guarantees,
}

/// 操作を許可するか判定する。
///
/// 判定は同期的で、外部の状態に依存しないことが望ましい（同じ入力には同じ判定を返す）。
pub trait PolicyEngine: Send + Sync {
    /// 判定する。
    fn decide(&self, ctx: &PolicyContext<'_>) -> Decision;
}

// ---------------------------------------------------------------------------
// 監査ログ
// ---------------------------------------------------------------------------

/// 監査ログのエラー。
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// 書き込みや読み込みに失敗した。
    #[error("監査ログの入出力に失敗しました: {0}")]
    Io(#[from] std::io::Error),
    /// 記録の形式が壊れている。
    #[error("監査ログの形式が不正です: {0}")]
    Format(String),
    /// ハッシュチェーンがつながっていない（改ざんや欠落の可能性）。
    #[error("監査ログのハッシュチェーンが壊れています（seq {seq}）")]
    Broken {
        /// 最初に不整合が見つかった記録の通し番号。
        seq: u64,
    },
}

/// 追記専用の監査ログ。
pub trait AuditSink: Send + Sync {
    /// 記録を追記し、封をした記録を返す。失敗したら操作は実行してはならない。
    fn append(&self, entry: AuditEntry) -> Result<AuditRecord, AuditError>;
}

// ---------------------------------------------------------------------------
// ツール
// ---------------------------------------------------------------------------

/// ツールの仕様。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// ツール名（例：`mail.read`）。
    pub name: String,
    /// 人間とモデル向けの説明。
    pub description: String,
    /// 呼び出しの影響範囲。
    pub effect: Effect,
    /// 出力に必ず付くラベル（例：Web の取得なら「信頼できない」）。
    pub output_labels: LabelSet,
    /// 引数の JSON Schema（モデルに見せる）。
    pub input_schema: serde_json::Value,
}

/// ツールの出力。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// 出力の内容。
    pub content: serde_json::Value,
    /// 内容に応じて追加で付くラベル。
    pub labels: LabelSet,
}

/// ツールのエラー。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 引数が不正。
    #[error("ツールの引数が不正です: {0}")]
    InvalidArgs(String),
    /// 実行に失敗した。
    #[error("ツールの実行に失敗しました: {0}")]
    Failed(String),
}

/// ツールの提供者。呼び出し元は常にブローカーで、エージェントが直接呼ぶことはない。
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// ツールの仕様。
    fn spec(&self) -> ToolSpec;
    /// ツールを実行する。
    async fn call(&self, args: serde_json::Value) -> Result<ToolOutput, ToolError>;
}

// ---------------------------------------------------------------------------
// モデル
// ---------------------------------------------------------------------------

/// モデルの能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCaps {
    /// モデルの ID。
    pub id: String,
    /// 端末の中で推論するか。`false` なら、入力は外部に送られる。
    pub local: bool,
    /// コンテキスト長（トークン数）。
    pub context_window: u32,
    /// ツール呼び出しに対応しているか。
    pub tool_use: bool,
}

impl ModelCaps {
    /// 推論の影響範囲。クラウドのモデルに入力を送るのは外部への作用として扱う。
    pub fn effect(&self) -> Effect {
        if self.local {
            Effect::Internal
        } else {
            Effect::External
        }
    }
}

/// メッセージの役割。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// システムの指示。
    System,
    /// ユーザー（またはエージェントの指示役）。
    #[default]
    User,
    /// モデルの応答。
    Assistant,
}

/// モデルに見せるツールの定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// 名前（英数字・`_`・`-` だけ。プロバイダの制約）。
    pub name: String,
    /// 説明。
    pub description: String,
    /// 引数の JSON Schema。
    pub input_schema: serde_json::Value,
}

/// モデルが求めたツールの呼び出し。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 呼び出しの ID（結果を返すときに使う）。
    pub id: String,
    /// ツール名。
    pub name: String,
    /// 引数。
    pub input: serde_json::Value,
}

/// ツールの呼び出しの結果（モデルに返す）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 対応する呼び出しの ID。
    pub call_id: String,
    /// 結果の本文。
    pub content: String,
    /// 失敗したか。
    #[serde(default)]
    pub is_error: bool,
}

/// 推論に渡すメッセージ。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// 役割。
    pub role: Role,
    /// 本文。
    pub content: String,
    /// モデル（assistant）が求めたツールの呼び出し。
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// ツールの呼び出しの結果（user）。
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
    /// プロバイダが返した応答をそのまま保持したもの（assistant）。
    /// 思考のブロックなど、次の要求で変えずに送り返す必要がある内容を含む。
    #[serde(default)]
    pub provider_raw: Option<serde_json::Value>,
}

impl Message {
    /// 文字列だけのメッセージを作る。
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            ..Self::default()
        }
    }
}

/// 推論の要求。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InferRequest {
    /// これまでの会話。
    pub messages: Vec<Message>,
    /// 応答の最大トークン数。
    pub max_tokens: u32,
    /// モデルが呼べるツール。
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

/// 応答が終わった理由。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 応答を終えた。
    #[default]
    EndTurn,
    /// ツールの呼び出しを求めている。
    ToolUse,
    /// 最大トークン数に達した（途中で切れている）。
    MaxTokens,
    /// その他。
    Other,
}

/// 推論の応答。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InferResponse {
    /// 応答の本文。
    pub content: String,
    /// ツールの呼び出し。
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// 終わった理由。
    #[serde(default)]
    pub stop_reason: StopReason,
    /// プロバイダが返した応答をそのまま保持したもの（次の要求の assistant に入れて送り返す）。
    #[serde(default)]
    pub provider_raw: Option<serde_json::Value>,
}

/// モデルのエラー。
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// プロバイダに接続できない。
    #[error("モデルに接続できません: {0}")]
    Unavailable(String),
    /// 推論に失敗した。
    #[error("推論に失敗しました: {0}")]
    Failed(String),
    /// モデル（または安全のための分類器）が応答を断った。
    #[error("モデルが応答を断りました（{0}）")]
    Refused(String),
}

/// モデルの提供者（Claude API、ローカルモデル、OpenAI 互換 API などを同じ形で扱う）。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// モデルの能力。
    fn caps(&self) -> ModelCaps;
    /// 推論する。
    async fn infer(&self, request: InferRequest) -> Result<InferResponse, ModelError>;
}
