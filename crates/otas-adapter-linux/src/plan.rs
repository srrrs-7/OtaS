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
    /// 名前空間を使う場合の計画（使わなければ `None`）。
    #[serde(default)]
    pub namespace: Option<NamespacePlan>,
    /// 入る cgroup（起動役が最初に自分を移す。子孫はすべてこの中で動く）。
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
}

/// 名前空間の中に見せるものの出どころ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanMountSource {
    /// ホストのパスをそのまま見せる（bind mount）。
    Bind {
        /// ホストのパス。
        path: PathBuf,
    },
    /// OverlayFS で重ねて見せる。
    Overlay {
        /// 下の層（読み取り専用）。
        lower: PathBuf,
        /// 上の層（変更が入る）。
        upper: PathBuf,
        /// 作業用ディレクトリ。
        work: PathBuf,
    },
}

/// 名前空間の中の `target` に見せるもの。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMount {
    /// 名前空間の中のパス。
    pub target: PathBuf,
    /// 出どころ。
    pub source: PlanMountSource,
    /// 書き込めるか。
    pub writable: bool,
}

/// 名前空間を使う場合の計画。
///
/// - root の方式：起動役は root で動き、準備が終わったら `uid` / `gid` に切り替える。
/// - root なしの方式（`rootless`）：起動役はユーザー名前空間を作り、その中の root（外から見るとデーモンと同じユーザー）
///   として準備をし、終わったらケーパビリティをすべて捨てる。`uid` / `gid` は使わない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespacePlan {
    /// 新しいルートを組み立てる空のディレクトリ（ホストのパス）。
    pub root: PathBuf,
    /// タスクを動かすユーザー。
    pub uid: u32,
    /// タスクを動かすグループ。
    pub gid: u32,
    /// 別のパスに見せるもの（`fs` の規則は同じパスで見せる）。
    pub mounts: Vec<PlanMount>,
    /// ユーザー名前空間を使い、root なしで動かすか。
    #[serde(default)]
    pub rootless: bool,
}
