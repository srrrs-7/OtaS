use crate::TaskId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// タスクがブローカーに要求する操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// データを読む。
    ReadData {
        /// 読む対象のパス（正本上の絶対パス）。
        path: String,
    },
    /// データを書く（作業領域への書き込み。確定はコミット時）。
    WriteData {
        /// 書く対象のパス（正本上の絶対パス）。
        path: String,
    },
    /// ツールを呼び出す。
    CallTool {
        /// ツール名。
        tool: String,
    },
    /// モデルで推論する。
    Infer {
        /// モデルの ID。
        model: String,
    },
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::ReadData { path } => write!(f, "読み取り {path}"),
            Action::WriteData { path } => write!(f, "書き込み {path}"),
            Action::CallTool { tool } => write!(f, "ツール {tool}"),
            Action::Infer { model } => write!(f, "推論 {model}"),
        }
    }
}

/// 操作の影響範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// OS の中で完結する。作業領域の変更はコミット前なら取り消せる。
    Internal,
    /// 外部に作用する。送信、クラウドへのデータ送信など、取り消せないものを含む。
    External,
}

/// ポリシーに判定を求める要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRequest {
    /// 要求したタスク。
    pub task: TaskId,
    /// 要求された操作。
    pub action: Action,
    /// 操作の影響範囲。
    pub effect: Effect,
}
