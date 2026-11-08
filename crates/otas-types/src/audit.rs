use crate::{Action, ApprovalId, Decision, Effect, LabelSet, TaskId};
use serde::{Deserialize, Serialize};

/// 操作がどうなったか。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// 許可され、これから実行する（実行前に必ず記録する）。
    Authorized,
    /// 実行が終わった。
    Executed,
    /// 実行したが失敗した。
    Failed {
        /// エラーの内容。
        error: String,
    },
    /// 拒否された。
    Denied,
    /// 人間の承認を待っている。
    Pending {
        /// 承認待ちの ID。
        approval: ApprovalId,
    },
    /// 人間が却下した。
    Rejected {
        /// 承認待ちの ID。
        approval: ApprovalId,
    },
}

/// 監査ログに書く内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 操作したタスク。
    pub task: TaskId,
    /// 操作。
    pub action: Action,
    /// 操作の影響範囲。
    pub effect: Effect,
    /// 判定した時点でタスクが持っていたラベル。
    pub labels: LabelSet,
    /// ポリシーの判定。
    pub decision: Decision,
    /// 人間が承認して実行したか。
    pub approved_by_human: bool,
    /// 結果。
    pub outcome: Outcome,
}

/// ハッシュチェーンで封をした監査ログの1行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// 0 から始まる通し番号。
    pub seq: u64,
    /// 記録した時刻（UNIX エポックからのミリ秒）。
    pub timestamp_ms: u64,
    /// 1つ前の記録のハッシュ（最初の記録は 0 が 64 桁）。
    pub prev_hash: String,
    /// この記録のハッシュ（SHA-256、16 進数）。
    pub hash: String,
    /// 記録の内容。
    pub entry: AuditEntry,
}
