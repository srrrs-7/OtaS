//! エージェント監督。タスクを作り、エージェントをサンドボックスで起動し、窓口を渡す。

use otas_broker::Broker;
use otas_ipc::{ConsoleBackend, Sessions, serve_task};
use otas_ports::{FsAccess, FsRule, NetPolicy, ResourceLimits, Sandbox, SandboxError, SandboxSpec};
use otas_types::{Grant, GrantSet, TaskId};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;

/// 資源の上限（依頼の JSON での形）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsRequest {
    /// CPU 時間（秒）。
    pub cpu_seconds: Option<u64>,
    /// アドレス空間（バイト）。
    pub address_space_bytes: Option<u64>,
    /// 書き込めるファイルの最大サイズ（バイト）。
    pub file_size_bytes: Option<u64>,
    /// 同時に開けるファイルの数。
    pub open_files: Option<u64>,
}

impl From<LimitsRequest> for ResourceLimits {
    fn from(l: LimitsRequest) -> Self {
        Self {
            cpu_seconds: l.cpu_seconds,
            address_space_bytes: l.address_space_bytes,
            file_size_bytes: l.file_size_bytes,
            open_files: l.open_files,
        }
    }
}

/// タスクの依頼。
///
/// サンドボックスで見えるファイルは、権限（`Read` と `Write`）から自動で決める。
/// 権限と見え方の基準を1つにするため、ファイルの規則を別に書く手段は用意しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequest {
    /// エージェントのプログラム（絶対パス）。
    pub program: PathBuf,
    /// 引数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 環境変数（これ以外は渡さない）。
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// タスクに与える権限。
    #[serde(default)]
    pub grants: GrantSet,
    /// 資源の上限。
    #[serde(default)]
    pub limits: LimitsRequest,
}

/// タスクの状態。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    /// 動いている。
    Running {
        /// プロセス ID。
        pid: u32,
    },
    /// 終了した。
    Exited {
        /// 終了コード。
        code: Option<i32>,
        /// 終了させたシグナル。
        signal: Option<i32>,
    },
    /// 起動できなかった。
    FailedToStart {
        /// 理由。
        error: String,
    },
    /// 終了を確かめられなかった。
    Unknown {
        /// 理由。
        error: String,
    },
}

/// 監督のエラー。
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// 依頼の形式が不正。
    #[error("依頼の形式が不正です: {0}")]
    InvalidRequest(String),
    /// サンドボックスで起動できない。
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// ソケットやログファイルの準備に失敗した。
    #[error("準備に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// 権限から、サンドボックスで見せるファイルの規則を作る。プログラム自体は読み取りと実行を許す。
pub fn fs_rules(grants: &GrantSet, program: &Path) -> Vec<FsRule> {
    let mut rules = vec![FsRule {
        path: program.to_path_buf(),
        access: FsAccess::ReadExec,
    }];
    for grant in grants.iter() {
        let (prefix, access) = match grant {
            Grant::Read { prefix } => (prefix, FsAccess::ReadOnly),
            Grant::Write { prefix } => (prefix, FsAccess::ReadWrite),
            Grant::Tool { .. } | Grant::Model { .. } => continue,
        };
        rules.push(FsRule {
            path: prefix.into(),
            access,
        });
    }
    rules
}

/// エージェント監督。tokio の `LocalSet` の中で使う。
pub struct Supervisor {
    broker: Arc<Broker>,
    sandbox: Arc<dyn Sandbox>,
    sessions: Sessions,
    task_dir: PathBuf,
    tasks: Rc<RefCell<BTreeMap<TaskId, TaskStatus>>>,
}

impl Supervisor {
    /// 監督を作る。`task_dir` にタスクごとの標準出力と標準エラー出力を置く。
    pub fn new(broker: Arc<Broker>, sandbox: Arc<dyn Sandbox>, task_dir: PathBuf) -> Self {
        Self {
            sessions: Sessions::new(broker.clone()),
            broker,
            sandbox,
            task_dir,
            tasks: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    /// タスクのログを置くディレクトリ。
    pub fn log_dir(&self, task: TaskId) -> PathBuf {
        self.task_dir.join(task.0.to_string())
    }

    /// タスクを作り、エージェントをサンドボックスで起動する。
    pub fn submit(&self, request: TaskRequest) -> Result<TaskId, SupervisorError> {
        if !request.program.is_absolute() {
            return Err(SupervisorError::InvalidRequest(
                "program は絶対パスで指定してください".into(),
            ));
        }
        let task = self.broker.spawn_task(request.grants.clone());

        // タスク専用の窓口の通り道。片方を子の stdin に、もう片方でブローカーを公開する
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let ours = tokio::net::UnixStream::from_std(ours)?;

        let dir = self.log_dir(task);
        std::fs::create_dir_all(&dir)?;
        let stdout = File::create(dir.join("stdout.log"))?;
        let stderr = File::create(dir.join("stderr.log"))?;

        let spec = SandboxSpec {
            fs: fs_rules(&request.grants, &request.program),
            program: request.program,
            args: request.args,
            env: request.env,
            cwd: None,
            net: NetPolicy::Deny,
            limits: request.limits.into(),
            stdin: Stdio::from(OwnedFd::from(theirs)),
            stdout: Stdio::from(stdout),
            stderr: Stdio::from(stderr),
        };

        let mut process = match self.sandbox.spawn(spec) {
            Ok(p) => p,
            Err(e) => {
                self.tasks.borrow_mut().insert(
                    task,
                    TaskStatus::FailedToStart {
                        error: e.to_string(),
                    },
                );
                return Err(e.into());
            }
        };
        self.tasks
            .borrow_mut()
            .insert(task, TaskStatus::Running { pid: process.id() });

        let server = serve_task(ours, &self.sessions, task);
        tokio::task::spawn_local(async move {
            // 接続が切れたら終わる（エージェントの終了など）。エラーは状態に反映しない
            let _ = server.await;
        });

        let tasks = self.tasks.clone();
        tokio::task::spawn_local(async move {
            let status = match tokio::task::spawn_blocking(move || process.wait()).await {
                Ok(Ok(info)) => TaskStatus::Exited {
                    code: info.code,
                    signal: info.signal,
                },
                Ok(Err(e)) => TaskStatus::Unknown {
                    error: e.to_string(),
                },
                Err(e) => TaskStatus::Unknown {
                    error: e.to_string(),
                },
            };
            tasks.borrow_mut().insert(task, status);
        });

        Ok(task)
    }

    /// タスクの一覧。
    pub fn tasks(&self) -> Vec<(TaskId, TaskStatus)> {
        self.tasks
            .borrow()
            .iter()
            .map(|(id, s)| (*id, s.clone()))
            .collect()
    }
}

impl ConsoleBackend for Supervisor {
    fn submit(&self, request_json: &str) -> Result<TaskId, String> {
        let request: TaskRequest =
            serde_json::from_str(request_json).map_err(|e| format!("依頼の形式が不正です: {e}"))?;
        Supervisor::submit(self, request).map_err(|e| e.to_string())
    }

    fn tasks(&self) -> Vec<(TaskId, String)> {
        Supervisor::tasks(self)
            .into_iter()
            .map(|(id, status)| {
                let json = serde_json::to_string(&status).unwrap_or_else(|_| "null".into());
                (id, json)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_rules_come_from_grants() {
        let grants = GrantSet::new([
            Grant::Read {
                prefix: "/data/mail".into(),
            },
            Grant::Write {
                prefix: "/data/projects/a".into(),
            },
            Grant::Tool {
                pattern: "*".into(),
            },
        ]);
        let rules = fs_rules(&grants, Path::new("/usr/libexec/agent"));
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].access, FsAccess::ReadExec);
        assert_eq!(rules[1].path, Path::new("/data/mail"));
        assert_eq!(rules[1].access, FsAccess::ReadOnly);
        assert_eq!(rules[2].access, FsAccess::ReadWrite);
    }

    #[test]
    fn request_json_is_strict() {
        let ok: TaskRequest = serde_json::from_str(
            r#"{"program":"/bin/agent","grants":[{"kind":"tool","pattern":"demo.*"}]}"#,
        )
        .unwrap();
        assert!(ok.args.is_empty());
        let unknown = serde_json::from_str::<TaskRequest>(r#"{"program":"/bin/agent","fs":[]}"#);
        assert!(unknown.is_err());
    }
}
