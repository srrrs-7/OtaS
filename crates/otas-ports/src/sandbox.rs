//! サンドボックスのポート。
//!
//! タスク（エージェントやツール）のプロセスを、見えるファイル・通信・システムコール・資源を
//! 制限した状態で起動する。実装（Linux の Landlock と seccomp、macOS の Seatbelt、VM など）は
//! 付け替えられる。実装は自分が保証できる範囲を [`Sandbox::guarantees`] で正直に申告する。

use otas_types::Guarantees;
use std::path::PathBuf;
use std::process::Stdio;

/// ファイルへのアクセスの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    /// 読み取りだけ（実行はできない）。
    ReadOnly,
    /// 読み取りと実行（プログラムやライブラリ用）。
    ReadExec,
    /// 読み書き（作成・削除を含む。実行はできない）。
    ReadWrite,
}

/// 1つのパス（とその配下）に許すアクセス。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRule {
    /// 絶対パス。
    pub path: PathBuf,
    /// 許すアクセス。
    pub access: FsAccess,
}

/// ネットワークの扱い。
///
/// タスクは外部と直接通信せず、通信はブローカー経由のツールが行う設計なので、現在は遮断だけを用意する。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetPolicy {
    /// すべて遮断する。
    #[default]
    Deny,
}

/// 資源の上限。`None` は「制限しない（実装の既定に従う）」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// CPU 時間（秒）。
    pub cpu_seconds: Option<u64>,
    /// アドレス空間（バイト）。
    pub address_space_bytes: Option<u64>,
    /// 書き込めるファイルの最大サイズ（バイト）。
    pub file_size_bytes: Option<u64>,
    /// 同時に開けるファイルの数。
    pub open_files: Option<u64>,
}

/// サンドボックスで起動するプロセスの指定。
#[derive(Debug)]
pub struct SandboxSpec {
    /// 実行するプログラムの絶対パス（PATH は探さない）。
    pub program: PathBuf,
    /// 引数。
    pub args: Vec<String>,
    /// 環境変数。ここに書いたものだけが渡り、親の環境変数は引き継がない。
    pub env: Vec<(String, String)>,
    /// 作業ディレクトリ。
    pub cwd: Option<PathBuf>,
    /// 許すファイルアクセス。書いていないパスには触れない。
    pub fs: Vec<FsRule>,
    /// ネットワークの扱い。
    pub net: NetPolicy,
    /// 資源の上限。
    pub limits: ResourceLimits,
    /// 標準入力。ブローカーとの IPC の通り道として使う。
    pub stdin: Stdio,
    /// 標準出力。
    pub stdout: Stdio,
    /// 標準エラー出力。
    pub stderr: Stdio,
}

impl SandboxSpec {
    /// プログラムを指定して作る。標準入出力は `null`、権限は何もない状態から始まる。
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            fs: Vec::new(),
            net: NetPolicy::Deny,
            limits: ResourceLimits::default(),
            stdin: Stdio::null(),
            stdout: Stdio::null(),
            stderr: Stdio::null(),
        }
    }

    /// 引数を追加する。
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// 環境変数を追加する。
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// ファイルアクセスを許す。
    pub fn allow(mut self, path: impl Into<PathBuf>, access: FsAccess) -> Self {
        self.fs.push(FsRule {
            path: path.into(),
            access,
        });
        self
    }
}

/// プロセスの終了状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// 終了コード（シグナルで終了した場合は `None`）。
    pub code: Option<i32>,
    /// 終了させたシグナルの番号。
    pub signal: Option<i32>,
}

impl ExitInfo {
    /// 終了コード 0 で終わったか。
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// サンドボックスのエラー。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// この環境ではサンドボックスを使えない。
    #[error("この環境ではサンドボックスを使えません: {0}")]
    Unsupported(String),
    /// 指定が不正。
    #[error("サンドボックスの指定が不正です: {0}")]
    InvalidSpec(String),
    /// プロセスの起動や操作に失敗した。
    #[error("プロセスの操作に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// サンドボックスで動いているプロセス。
pub trait SandboxProcess: Send {
    /// プロセス ID。
    fn id(&self) -> u32;
    /// 終了を待つ。
    fn wait(&mut self) -> Result<ExitInfo, SandboxError>;
    /// 終了していれば終了状態を返す（待たない）。
    fn try_wait(&mut self) -> Result<Option<ExitInfo>, SandboxError>;
    /// 強制終了する。
    fn kill(&mut self) -> Result<(), SandboxError>;
}

/// プロセスを制限した状態で起動する。
pub trait Sandbox: Send + Sync {
    /// この実装が保証できる範囲。
    fn guarantees(&self) -> Guarantees;
    /// プロセスを起動する。
    fn spawn(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxProcess>, SandboxError>;
}
