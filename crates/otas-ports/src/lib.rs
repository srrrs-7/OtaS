//! OtaS のポート（trait）定義。
//!
//! 中核（ブローカーなど）はこのクレートの trait だけに依存する。
//! ポリシー、監査ログ、ツール、モデルの実装はすべて付け替えられる。
//!
//! MVP では、ブローカーが使う trait だけを定義する。
//! サンドボックス、作業領域、イベントバス、通知などの trait は次の段階で追加する
//! （`docs/design/001-architecture.md` §4）。

use async_trait::async_trait;
use otas_types::{
    AccessRequest, AuditEntry, AuditRecord, Decision, Effect, GrantSet, Guarantees, LabelSet,
};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// ツール名（例：`mail.read`）。
    pub name: String,
    /// 人間とモデル向けの説明。
    pub description: String,
    /// 呼び出しの影響範囲。
    pub effect: Effect,
    /// 出力に必ず付くラベル（例：Web の取得なら「信頼できない」）。
    pub output_labels: LabelSet,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// システムの指示。
    System,
    /// ユーザー（またはエージェントの指示役）。
    User,
    /// モデルの応答。
    Assistant,
}

/// 推論に渡すメッセージ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 役割。
    pub role: Role,
    /// 本文。
    pub content: String,
}

/// 推論の要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferRequest {
    /// これまでの会話。
    pub messages: Vec<Message>,
    /// 応答の最大トークン数。
    pub max_tokens: u32,
}

/// 推論の応答。ストリーミングは次の段階で追加する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferResponse {
    /// 応答の本文。
    pub content: String,
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
}

/// モデルの提供者（Claude API、ローカルモデル、OpenAI 互換 API などを同じ形で扱う）。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// モデルの能力。
    fn caps(&self) -> ModelCaps;
    /// 推論する。
    async fn infer(&self, request: InferRequest) -> Result<InferResponse, ModelError>;
}
