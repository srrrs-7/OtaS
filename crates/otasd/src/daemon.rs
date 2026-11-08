//! デーモン本体。部品を組み立て、人間向けの窓口で待ち受ける。

use crate::config::Config;
use crate::demo;
use crate::supervisor::Supervisor;
use otas_audit::FileAuditLog;
use otas_broker::Broker;
use otas_ipc::{ConsoleBackend, serve_console, verify_peer_uid};
use otas_policy::DefaultPolicy;
use otas_ports::{AuditError, Sandbox, StateStore, Workspace, WorkspaceError};
use otas_state::FileStateStore;
use otas_workspace::{FsStore, StoreOptions, ViewMode};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::rc::Rc;
use std::sync::Arc;
use tokio::net::UnixListener;

/// デーモンのエラー。
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// 監査ログを開けない（改ざんを検知した場合を含む）。
    #[error(transparent)]
    Audit(#[from] AuditError),
    /// 作業領域を開けない。
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// 保存した状態を読み込めない。
    #[error(transparent)]
    State(#[from] otas_ports::StateError),
    /// ブローカーの状態を読み戻せない。
    #[error(transparent)]
    Broker(#[from] otas_broker::BrokerError),
    /// タスクの状態を読み戻せない。
    #[error(transparent)]
    Supervisor(#[from] crate::supervisor::SupervisorError),
    /// サンドボックスを使えない。
    #[error(transparent)]
    Sandbox(#[from] otas_ports::SandboxError),
    /// 設定が不正。
    #[error("設定が不正です: {0}")]
    Config(String),
    /// モデルの設定が不正、または API キーが見つからない。
    #[error(transparent)]
    Model(#[from] otas_models::ModelConfigError),
    /// ディレクトリやソケットの操作に失敗した。
    #[error("入出力に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// デーモン。tokio の `LocalSet` の中で使う。
pub struct Daemon {
    config: Config,
    broker: Arc<Broker>,
    supervisor: Rc<Supervisor>,
    store: Arc<FsStore>,
}

impl Daemon {
    /// 部品を組み立てる。状態ディレクトリは所有者だけが使える権限（0700）にする。
    pub fn new(config: Config, sandbox: Arc<dyn Sandbox>) -> Result<Self, DaemonError> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.state_dir)?;
        std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700))?;

        let audit = Arc::new(FileAuditLog::open(config.audit_log())?);
        // 名前空間のサンドボックスなら OverlayFS の作業領域（/data として見せる）、そうでなければコピー方式
        let options = StoreOptions {
            view: if sandbox.supports_mounts() {
                ViewMode::Overlay
            } else {
                ViewMode::Copy
            },
            versions: config.data.versions,
            owner: config.task_owner(),
        };
        let store = Arc::new(FsStore::open_with(config.store_dir(), options)?);
        for rule in &config.data.labels {
            store.add_labels(&rule.path, &rule.labels)?;
        }

        // 環境の保証は、サンドボックスと作業領域の申告を合わせたもの
        let mut guarantees = sandbox.guarantees();
        guarantees.snapshot = store.snapshot_level();

        let policy = Arc::new(DefaultPolicy::new(config.policy.external_requires));
        let mut builder = Broker::builder(policy, audit).guarantees(guarantees);
        if config.demo.tools {
            for tool in demo::tools() {
                builder = builder.tool(tool);
            }
        }
        for model in &config.models {
            builder = builder.model(otas_models::build_from_env(model)?);
        }
        // タスクの権限とラベル、承認待ち、タスクの状態を保存し、再起動しても続きから動けるようにする
        let state: Arc<dyn StateStore> =
            Arc::new(FileStateStore::open(config.state_dir.join("state"))?);
        let broker = Arc::new(builder.state(state.clone()).build());
        broker.restore()?;
        let supervisor = Rc::new(Supervisor::new(
            broker.clone(),
            sandbox,
            store.clone(),
            config.task_dir(),
            Some(state),
        ));
        supervisor.restore()?;
        Ok(Self {
            config,
            broker,
            supervisor,
            store,
        })
    }

    /// 正本の置き場所（論理パス `/data` に当たる）。
    pub fn data_dir(&self) -> std::path::PathBuf {
        self.store.live_dir()
    }

    /// ブローカー。
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// エージェント監督。
    pub fn supervisor(&self) -> &Rc<Supervisor> {
        &self.supervisor
    }

    /// 人間向けの窓口のソケットを作る。前回の残りのソケットがあれば消す（ソケット以外は消さない）。
    pub fn bind(&self) -> Result<UnixListener, DaemonError> {
        let path = self.config.console_socket();
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_socket() {
                std::fs::remove_file(&path)?;
            }
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// 人間向けの窓口で待ち受ける。止めるまで戻らない。
    /// 接続元がデーモンと同じユーザーでなければ、窓口を渡さずに切る。
    pub async fn serve(&self, listener: UnixListener) -> Result<(), DaemonError> {
        let uid = rustix::process::getuid().as_raw();
        loop {
            let (stream, _) = listener.accept().await?;
            if let Err(e) = verify_peer_uid(&stream, uid) {
                eprintln!("otasd: 接続を拒否しました: {e}");
                continue;
            }
            let backend: Rc<dyn ConsoleBackend> = self.supervisor.clone();
            let server = serve_console(stream, backend, self.broker.clone());
            tokio::task::spawn_local(async move {
                let _ = server.await;
            });
        }
    }
}

/// 設定に従ってサンドボックスを作る。
pub fn sandbox_from_config(config: &Config) -> Result<Arc<dyn Sandbox>, DaemonError> {
    use crate::config::SandboxMode;
    use otas_adapter_linux::{LinuxSandbox, NamespaceConfig};
    let sandbox = match config.sandbox.mode {
        SandboxMode::Landlock => LinuxSandbox::detect(&config.launcher)?,
        SandboxMode::Rootless => LinuxSandbox::detect_rootless(&config.launcher, config.run_dir())?,
        SandboxMode::Namespace => {
            let (uid, gid) = config.task_owner().ok_or_else(|| {
                DaemonError::Config("namespace では [sandbox] の uid と gid が必要です".into())
            })?;
            LinuxSandbox::detect_namespaced(
                &config.launcher,
                NamespaceConfig {
                    uid,
                    gid,
                    run_dir: config.run_dir(),
                },
            )?
        }
    };
    let sandbox = match &config.sandbox.cgroup {
        Some(base) => sandbox.with_cgroups(base)?,
        None => sandbox,
    };
    Ok(Arc::new(sandbox))
}
