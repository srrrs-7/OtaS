//! 状態の保存のポート。
//!
//! ブローカー（タスクの権限・ラベル・承認待ち）や監督（タスクの状態）が、
//! デーモンを再起動しても続きから動けるように、状態をキーごとに保存する。
//! 実装（ファイル、SQLite など）は付け替えられる。

/// 状態の保存のエラー。
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// 入出力に失敗した。
    #[error("状態を保存または読み込みできません: {0}")]
    Io(#[from] std::io::Error),
    /// キーが不正。
    #[error("状態のキーが不正です: {0}")]
    InvalidKey(String),
    /// 保存された内容が壊れている。
    #[error("保存された状態が壊れています: {0}")]
    Corrupt(String),
}

/// キーごとに状態を保存する。保存は丸ごと置き換え（途中までの書き込みが残らないこと）。
pub trait StateStore: Send + Sync {
    /// 読み込む。保存されていなければ `None`。
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, StateError>;
    /// 保存する（前の内容を丸ごと置き換える）。
    fn save(&self, key: &str, value: &[u8]) -> Result<(), StateError>;
}
