//! 作業領域のポート。
//!
//! タスクの書き込みはすぐに正本へ反映せず、タスク専用の作業領域にためる。
//! 人間（またはポリシー）が差分を確認してから、正本にコミットするか破棄する。
//! 実装（ハードリンクによる版管理、Btrfs と OverlayFS など）は付け替えられる。
//!
//! パスは正本上の論理パス（`/data/...`）で表す。物理的な置き場所は実装が決める。

use crate::MountSource;
use otas_types::{LabelSet, Level, TaskId};
use std::path::PathBuf;

/// 作業領域に見せる範囲（正本上の論理パス）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewSpec {
    /// 読み取りだけを許す範囲。
    pub read: Vec<String>,
    /// 読み書きを許す範囲（変更はコミットするまで正本に反映されない）。
    pub write: Vec<String>,
}

/// 作業領域に見せた1つの範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    /// 正本上の論理パス。
    pub logical: String,
    /// 出どころ。コピーやハードリンクなら [`MountSource::Path`]、OverlayFS なら [`MountSource::Overlay`]。
    pub source: MountSource,
    /// 書き込めるか。
    pub writable: bool,
    /// この範囲のデータに付いているラベル。
    pub labels: LabelSet,
}

/// 作られた作業領域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceView {
    /// 作業領域の根（正本の `/data` に当たる）。
    pub root: PathBuf,
    /// 作業領域の元にした版。
    pub base_version: u64,
    /// 見せた範囲。
    pub entries: Vec<ViewEntry>,
}

/// 変更の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 追加。
    Added,
    /// 変更。
    Modified,
    /// 削除。
    Deleted,
}

/// 1つのファイルの変更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// 正本上の論理パス。
    pub path: String,
    /// 変更の種類。
    pub kind: ChangeKind,
}

/// コミットの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// 正本に反映し、新しい版を作った。
    Applied {
        /// 新しい版。
        version: u64,
        /// 反映した変更。
        changes: Vec<Change>,
    },
    /// 作業領域を作った後に、同じファイルが正本で変わっていた。何も反映していない。
    Conflict {
        /// 衝突したパス。
        paths: Vec<String>,
    },
    /// 変更がなかった。
    NoChanges,
}

/// 作業領域のエラー。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// そのタスクの作業領域がない。
    #[error("タスク {0} の作業領域はありません")]
    NotFound(TaskId),
    /// そのタスクの作業領域はすでにある。
    #[error("タスク {0} の作業領域はすでにあります")]
    AlreadyExists(TaskId),
    /// その版はない。
    #[error("版 {0} はありません")]
    NoSuchVersion(u64),
    /// パスが不正。
    #[error("パスが不正です: {0}")]
    InvalidPath(String),
    /// 扱えない内容（シンボリックリンクなど、通常のファイルとディレクトリ以外）。
    #[error("扱えない内容です: {0}")]
    Unsupported(String),
    /// 管理用のデータが壊れている。
    #[error("作業領域の管理データが壊れています: {0}")]
    Corrupt(String),
    /// 入出力に失敗した。
    #[error("入出力に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// 作業領域と、正本の版を管理する。
pub trait Workspace: Send + Sync {
    /// 版（スナップショット）の保証の強さ。
    fn snapshot_level(&self) -> Level;
    /// 現在の版。
    fn current_version(&self) -> Result<u64, WorkspaceError>;
    /// 残っている版の一覧（古い順）。
    fn versions(&self) -> Result<Vec<u64>, WorkspaceError>;
    /// タスクの作業領域を、現在の版から作る。
    fn begin(&self, task: TaskId, view: &ViewSpec) -> Result<WorkspaceView, WorkspaceError>;
    /// 作業領域の変更（元にした版との差分）。
    fn diff(&self, task: TaskId) -> Result<Vec<Change>, WorkspaceError>;
    /// 作業領域の変更を正本に反映する。書いたファイルには `labels` を付ける。
    /// 衝突があれば何も反映しない。反映したら作業領域は片付ける。
    fn commit(&self, task: TaskId, labels: &LabelSet) -> Result<CommitOutcome, WorkspaceError>;
    /// 作業領域を捨てる。
    fn discard(&self, task: TaskId) -> Result<(), WorkspaceError>;
    /// 正本を指定した版の状態に戻す。戻した状態を新しい版として記録し、その版を返す。
    fn rollback(&self, to: u64) -> Result<u64, WorkspaceError>;
}
