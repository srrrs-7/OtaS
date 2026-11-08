use serde::{Deserialize, Serialize};

/// ポリシーの判定結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// 許可する。
    Allow,
    /// 拒否する。
    Deny {
        /// 人間が読める理由。
        reason: String,
    },
    /// 人間の承認を求める。承認されるまで実行しない。
    AskHuman {
        /// 人間が読める理由。
        reason: String,
    },
}
