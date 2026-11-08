//! 親から起動役に渡す起動計画。起動役のコマンドライン引数（JSON）として渡す。

use otas_ports::{FsAccess, ResourceLimits};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ファイルへのアクセスの種類（[`FsAccess`] と同じ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAccess {
    /// 読み取りだけ。
    ReadOnly,
    /// 読み取りと実行。
    ReadExec,
    /// 読み書き（実行はできない）。
    ReadWrite,
}

impl From<FsAccess> for PlanAccess {
    fn from(access: FsAccess) -> Self {
        match access {
            FsAccess::ReadOnly => PlanAccess::ReadOnly,
            FsAccess::ReadExec => PlanAccess::ReadExec,
            FsAccess::ReadWrite => PlanAccess::ReadWrite,
        }
    }
}

/// 1つのパスに許すアクセス。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFsRule {
    /// 絶対パス。
    pub path: PathBuf,
    /// 許すアクセス。
    pub access: PlanAccess,
    /// パスが存在しなければ無視してよいか（システムのパスなど）。
    /// `false` のパスが存在しなければ、起動を拒否する。
    #[serde(default)]
    pub optional: bool,
}

/// 資源の上限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLimits {
    /// CPU 時間（秒）。
    pub cpu_seconds: Option<u64>,
    /// アドレス空間（バイト）。
    pub address_space_bytes: Option<u64>,
    /// 書き込めるファイルの最大サイズ（バイト）。
    pub file_size_bytes: Option<u64>,
    /// 同時に開けるファイルの数。
    pub open_files: Option<u64>,
}

impl From<ResourceLimits> for PlanLimits {
    fn from(l: ResourceLimits) -> Self {
        Self {
            cpu_seconds: l.cpu_seconds,
            address_space_bytes: l.address_space_bytes,
            file_size_bytes: l.file_size_bytes,
            open_files: l.open_files,
        }
    }
}

/// 起動計画。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// 実行するプログラムの絶対パス。
    pub program: PathBuf,
    /// 引数。
    pub args: Vec<String>,
    /// 渡す環境変数（これ以外は渡さない）。
    pub env: Vec<(String, String)>,
    /// 作業ディレクトリ。
    pub cwd: Option<PathBuf>,
    /// 許すファイルアクセス。
    pub fs: Vec<PlanFsRule>,
    /// ネットワークを遮断するか。
    pub deny_network: bool,
    /// 資源の上限。
    pub limits: PlanLimits,
}
