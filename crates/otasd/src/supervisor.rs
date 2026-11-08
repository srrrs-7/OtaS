//! エージェント監督。タスクを作り、作業領域を用意し、エージェントをサンドボックスで起動して窓口を渡す。

use otas_broker::{Broker, BrokerError, CallResult};
use otas_ipc::{ConsoleBackend, Sessions, serve_task};
use otas_ports::{
    Change, ChangeKind, CommitOutcome, FsAccess, FsRule, NetPolicy, ResourceLimits, Sandbox,
    SandboxError, SandboxSpec, StateError, StateStore, ViewSpec, Workspace, WorkspaceError,
    WorkspaceView,
};
use otas_types::{Grant, GrantSet, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;

/// エージェントに作業領域の場所を知らせる環境変数（正本の `/data` に当たる）。
pub const DATA_ENV: &str = "OTAS_DATA";

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
/// 作業領域に見せる範囲と、サンドボックスで見えるファイルは、権限（`read` と `write`）から自動で決める。
/// 権限と見え方の基準を1つにするため、ファイルの規則を別に書く手段は用意しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequest {
    /// エージェントのプログラム（絶対パス）。
    pub program: PathBuf,
    /// 引数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 環境変数（これと `OTAS_DATA` 以外は渡さない）。
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// タスクに与える権限。`read` と `write` のパスは `/data` 以下に限る。
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
    /// 終了した（作業領域に変更はなかった）。
    Exited {
        /// 終了コード。
        code: Option<i32>,
        /// 終了させたシグナル。
        signal: Option<i32>,
    },
    /// 終了し、作業領域の変更がレビューを待っている。
    AwaitingReview {
        /// 終了コード。
        code: Option<i32>,
        /// 終了させたシグナル。
        signal: Option<i32>,
        /// 変更されたファイルの数。
        changes: usize,
    },
    /// 変更を正本にコミットした。
    Committed {
        /// 反映した版。
        version: u64,
    },
    /// 変更を捨てた。
    Discarded,
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
    /// デーモンの再起動で中断した（作業領域に変更はなかった）。
    Interrupted,
}

/// 状態を保存するときのキー。
pub const STATE_KEY: &str = "supervisor";

/// タスクの状態の一覧。変わるたびに保存する。
struct TaskTable {
    map: RefCell<BTreeMap<TaskId, TaskStatus>>,
    store: Option<Arc<dyn StateStore>>,
}

impl TaskTable {
    fn get(&self, task: TaskId) -> Option<TaskStatus> {
        self.map.borrow().get(&task).cloned()
    }

    fn set(&self, task: TaskId, status: TaskStatus) {
        self.map.borrow_mut().insert(task, status);
        self.save();
    }

    /// 保存に失敗しても動作は続ける。状態は表示のためのもので、作業領域とブローカーの状態は別に保存されている。
    fn save(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let result = serde_json::to_vec(&*self.map.borrow())
            .map_err(|e| e.to_string())
            .and_then(|bytes| store.save(STATE_KEY, &bytes).map_err(|e| e.to_string()));
        if let Err(e) = result {
            eprintln!("otasd: タスクの状態を保存できません: {e}");
        }
    }
}

/// 監督のエラー。
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// 依頼の形式が不正。
    #[error("依頼の形式が不正です: {0}")]
    InvalidRequest(String),
    /// 今の状態ではできない操作。
    #[error("{0}")]
    InvalidState(String),
    /// ブローカーに拒否された。
    #[error("拒否されました: {0}")]
    Denied(String),
    /// ブローカーのエラー。
    #[error(transparent)]
    Broker(#[from] BrokerError),
    /// 状態を読み込めない。
    #[error(transparent)]
    State(#[from] StateError),
    /// 作業領域のエラー。
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// サンドボックスで起動できない。
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// ソケットやログファイルの準備に失敗した。
    #[error("準備に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// 権限から、作業領域に見せる範囲を作る。
pub fn view_spec(grants: &GrantSet) -> ViewSpec {
    let mut view = ViewSpec::default();
    for grant in grants.iter() {
        match grant {
            Grant::Read { prefix } => view.read.push(prefix.clone()),
            Grant::Write { prefix } => view.write.push(prefix.clone()),
            Grant::Tool { .. } | Grant::Model { .. } => {}
        }
    }
    view
}

/// 作業領域から、サンドボックスで見せるファイルの規則を作る。プログラム自体は読み取りと実行を許す。
pub fn fs_rules(view: &WorkspaceView, program: &Path) -> Vec<FsRule> {
    let mut rules = vec![FsRule {
        path: program.to_path_buf(),
        access: FsAccess::ReadExec,
    }];
    for entry in &view.entries {
        rules.push(FsRule {
            path: entry.physical.clone(),
            access: if entry.writable {
                FsAccess::ReadWrite
            } else {
                FsAccess::ReadOnly
            },
        });
    }
    rules
}

fn change_json(change: &Change) -> Value {
    let kind = match change.kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    };
    json!({ "path": change.path, "kind": kind })
}

fn outcome_json(outcome: &CommitOutcome) -> Value {
    match outcome {
        CommitOutcome::Applied { version, changes } => json!({
            "result": "applied",
            "version": version,
            "changes": changes.iter().map(change_json).collect::<Vec<_>>(),
        }),
        CommitOutcome::Conflict { paths } => json!({ "result": "conflict", "paths": paths }),
        CommitOutcome::NoChanges => json!({ "result": "no_changes" }),
    }
}

/// エージェント監督。tokio の `LocalSet` の中で使う。
pub struct Supervisor {
    broker: Arc<Broker>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Arc<dyn Workspace>,
    sessions: Sessions,
    task_dir: PathBuf,
    tasks: Rc<TaskTable>,
}

impl Supervisor {
    /// 監督を作る。`task_dir` にタスクごとの標準出力と標準エラー出力を置く。
    /// `store` を渡すと、タスクの状態を保存する（読み戻すには [`Supervisor::restore`] を呼ぶ）。
    pub fn new(
        broker: Arc<Broker>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Arc<dyn Workspace>,
        task_dir: PathBuf,
        store: Option<Arc<dyn StateStore>>,
    ) -> Self {
        Self {
            sessions: Sessions::new(broker.clone()),
            broker,
            sandbox,
            workspace,
            task_dir,
            tasks: Rc::new(TaskTable {
                map: RefCell::new(BTreeMap::new()),
                store,
            }),
        }
    }

    /// 保存したタスクの状態を読み戻す。デーモンの起動時に1回呼ぶ。
    ///
    /// 動いていたタスクのエージェントは、デーモンと一緒に止まっている。
    /// 作業領域に変更が残っていればレビュー待ちに、なければ中断として扱う。
    pub fn restore(&self) -> Result<(), SupervisorError> {
        let Some(store) = &self.tasks.store else {
            return Ok(());
        };
        let Some(bytes) = store.load(STATE_KEY)? else {
            return Ok(());
        };
        let mut map: BTreeMap<TaskId, TaskStatus> = serde_json::from_slice(&bytes)
            .map_err(|e| StateError::Corrupt(format!("タスクの状態を読めません: {e}")))?;
        for (task, status) in map.iter_mut() {
            if !matches!(status, TaskStatus::Running { .. }) {
                continue;
            }
            *status = match self.workspace.diff(*task) {
                Ok(changes) if changes.is_empty() => {
                    let _ = self.workspace.discard(*task);
                    TaskStatus::Interrupted
                }
                Ok(changes) => TaskStatus::AwaitingReview {
                    code: None,
                    signal: None,
                    changes: changes.len(),
                },
                Err(WorkspaceError::NotFound(_)) => TaskStatus::Interrupted,
                Err(e) => TaskStatus::Unknown {
                    error: e.to_string(),
                },
            };
        }
        *self.tasks.map.borrow_mut() = map;
        self.tasks.save();
        Ok(())
    }

    /// タスクのログを置くディレクトリ。
    pub fn log_dir(&self, task: TaskId) -> PathBuf {
        self.task_dir.join(task.0.to_string())
    }

    fn status(&self, task: TaskId) -> Option<TaskStatus> {
        self.tasks.get(task)
    }

    fn set_status(&self, task: TaskId, status: TaskStatus) {
        self.tasks.set(task, status);
    }

    /// 起動の途中で失敗したときの後片付け。
    fn fail_start(&self, task: TaskId, error: SupervisorError) -> SupervisorError {
        let _ = self.workspace.discard(task);
        self.set_status(
            task,
            TaskStatus::FailedToStart {
                error: error.to_string(),
            },
        );
        error
    }

    /// タスクを作り、作業領域を用意して、エージェントをサンドボックスで起動する。
    pub fn submit(&self, request: TaskRequest) -> Result<TaskId, SupervisorError> {
        if !request.program.is_absolute() {
            return Err(SupervisorError::InvalidRequest(
                "program は絶対パスで指定してください".into(),
            ));
        }
        let task = self.broker.spawn_task(request.grants.clone());

        let view = match self.workspace.begin(task, &view_spec(&request.grants)) {
            Ok(v) => v,
            Err(e) => return Err(self.fail_start(task, e.into())),
        };

        // 作業領域に見せるデータは、タスクが読んだものとして扱う（ラベルをタスクに取り込む）
        for entry in &view.entries {
            match self.broker.read_data(task, &entry.logical, &entry.labels) {
                Ok(CallResult::Done(())) => {}
                Ok(CallResult::Pending(_)) => {
                    let e = SupervisorError::Denied(format!(
                        "{} を見せるには承認が必要です",
                        entry.logical
                    ));
                    return Err(self.fail_start(task, e));
                }
                Err(e) => return Err(self.fail_start(task, e.into())),
            }
        }

        match self.launch(task, request, &view) {
            Ok(()) => Ok(task),
            Err(e) => Err(self.fail_start(task, e)),
        }
    }

    fn launch(
        &self,
        task: TaskId,
        request: TaskRequest,
        view: &WorkspaceView,
    ) -> Result<(), SupervisorError> {
        // タスク専用の窓口の通り道。片方を子の stdin に、もう片方でブローカーを公開する
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let ours = tokio::net::UnixStream::from_std(ours)?;

        let dir = self.log_dir(task);
        std::fs::create_dir_all(&dir)?;
        let stdout = File::create(dir.join("stdout.log"))?;
        let stderr = File::create(dir.join("stderr.log"))?;

        let mut env = request.env;
        env.push((DATA_ENV.into(), view.root.display().to_string()));
        let spec = SandboxSpec {
            fs: fs_rules(view, &request.program),
            program: request.program,
            args: request.args,
            env,
            cwd: None,
            net: NetPolicy::Deny,
            limits: request.limits.into(),
            stdin: Stdio::from(OwnedFd::from(theirs)),
            stdout: Stdio::from(stdout),
            stderr: Stdio::from(stderr),
        };
        let mut process = self.sandbox.spawn(spec)?;
        self.set_status(task, TaskStatus::Running { pid: process.id() });

        let server = serve_task(ours, &self.sessions, task);
        tokio::task::spawn_local(async move {
            // 接続が切れたら終わる（エージェントの終了など）。エラーは状態に反映しない
            let _ = server.await;
        });

        let tasks = self.tasks.clone();
        let workspace = self.workspace.clone();
        tokio::task::spawn_local(async move {
            let status = match tokio::task::spawn_blocking(move || process.wait()).await {
                Ok(Ok(info)) => match workspace.diff(task) {
                    Ok(changes) if changes.is_empty() => {
                        let _ = workspace.discard(task);
                        TaskStatus::Exited {
                            code: info.code,
                            signal: info.signal,
                        }
                    }
                    Ok(changes) => TaskStatus::AwaitingReview {
                        code: info.code,
                        signal: info.signal,
                        changes: changes.len(),
                    },
                    Err(e) => TaskStatus::Unknown {
                        error: e.to_string(),
                    },
                },
                Ok(Err(e)) => TaskStatus::Unknown {
                    error: e.to_string(),
                },
                Err(e) => TaskStatus::Unknown {
                    error: e.to_string(),
                },
            };
            tasks.set(task, status);
        });
        Ok(())
    }

    /// タスクの一覧。
    pub fn tasks(&self) -> Vec<(TaskId, TaskStatus)> {
        self.tasks
            .map
            .borrow()
            .iter()
            .map(|(id, s)| (*id, s.clone()))
            .collect()
    }

    fn require_review(&self, task: TaskId) -> Result<(), SupervisorError> {
        match self.status(task) {
            Some(TaskStatus::AwaitingReview { .. }) => Ok(()),
            Some(TaskStatus::Running { .. }) => Err(SupervisorError::InvalidState(
                "タスクがまだ動いています。終わってからレビューしてください".into(),
            )),
            Some(other) => Err(SupervisorError::InvalidState(format!(
                "レビュー待ちの変更はありません（状態: {other:?}）"
            ))),
            None => Err(SupervisorError::InvalidState(format!(
                "タスク {} はありません",
                task.0
            ))),
        }
    }

    /// 作業領域の変更。
    pub fn review(&self, task: TaskId) -> Result<Vec<Change>, SupervisorError> {
        self.require_review(task)?;
        Ok(self.workspace.diff(task)?)
    }

    /// 作業領域の変更を正本にコミットする。変更する各パスについてブローカーで判定し、記録する。
    pub fn commit(&self, task: TaskId) -> Result<CommitOutcome, SupervisorError> {
        self.require_review(task)?;
        for change in self.workspace.diff(task)? {
            match self.broker.write_data(task, &change.path)? {
                CallResult::Done(()) => {}
                CallResult::Pending(_) => {
                    return Err(SupervisorError::Denied(format!(
                        "{} への書き込みには承認が必要です",
                        change.path
                    )));
                }
            }
        }
        let labels = self.broker.labels(task)?;
        let outcome = self.workspace.commit(task, &labels)?;
        match &outcome {
            CommitOutcome::Applied { version, .. } => {
                self.set_status(task, TaskStatus::Committed { version: *version });
            }
            CommitOutcome::NoChanges => self.set_status(task, TaskStatus::Discarded),
            CommitOutcome::Conflict { .. } => {}
        }
        Ok(outcome)
    }

    /// 作業領域の変更を捨てる。
    pub fn discard(&self, task: TaskId) -> Result<(), SupervisorError> {
        self.require_review(task)?;
        self.workspace.discard(task)?;
        self.set_status(task, TaskStatus::Discarded);
        Ok(())
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

    fn review(&self, task: TaskId) -> Result<String, String> {
        let changes = Supervisor::review(self, task).map_err(|e| e.to_string())?;
        Ok(Value::from(changes.iter().map(change_json).collect::<Vec<_>>()).to_string())
    }

    fn commit(&self, task: TaskId) -> Result<String, String> {
        let outcome = Supervisor::commit(self, task).map_err(|e| e.to_string())?;
        Ok(outcome_json(&outcome).to_string())
    }

    fn discard(&self, task: TaskId) -> Result<(), String> {
        Supervisor::discard(self, task).map_err(|e| e.to_string())
    }

    fn versions(&self) -> Result<String, String> {
        let current = self
            .workspace
            .current_version()
            .map_err(|e| e.to_string())?;
        let versions = self.workspace.versions().map_err(|e| e.to_string())?;
        Ok(json!({ "current": current, "versions": versions }).to_string())
    }

    fn rollback(&self, version: u64) -> Result<String, String> {
        let new = self
            .workspace
            .rollback(version)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "version": new }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otas_ports::ViewEntry;
    use otas_types::LabelSet;

    #[test]
    fn the_view_comes_from_grants() {
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
        let view = view_spec(&grants);
        assert_eq!(view.read, vec!["/data/mail"]);
        assert_eq!(view.write, vec!["/data/projects/a"]);
    }

    #[test]
    fn file_rules_follow_the_workspace() {
        let view = WorkspaceView {
            root: "/state/work/t/view".into(),
            base_version: 1,
            entries: vec![
                ViewEntry {
                    logical: "/data/mail".into(),
                    physical: "/state/work/t/view/mail".into(),
                    writable: false,
                    labels: LabelSet::empty(),
                },
                ViewEntry {
                    logical: "/data/projects/a".into(),
                    physical: "/state/work/t/view/projects/a".into(),
                    writable: true,
                    labels: LabelSet::empty(),
                },
            ],
        };
        let rules = fs_rules(&view, Path::new("/usr/libexec/agent"));
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].access, FsAccess::ReadExec);
        assert_eq!(rules[1].access, FsAccess::ReadOnly);
        assert_eq!(rules[2].access, FsAccess::ReadWrite);
        assert_eq!(rules[2].path, Path::new("/state/work/t/view/projects/a"));
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
