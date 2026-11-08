//! デーモン本体。部品を組み立て、人間向けの窓口で待ち受ける。

use crate::config::Config;
use crate::demo;
use crate::supervisor::Supervisor;
use otas_audit::FileAuditLog;
use otas_broker::Broker;
use otas_ipc::{ConsoleBackend, serve_console, verify_peer_uid};
use otas_policy::DefaultPolicy;
use otas_ports::{AuditError, Sandbox};
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
    /// ディレクトリやソケットの操作に失敗した。
    #[error("入出力に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// デーモン。tokio の `LocalSet` の中で使う。
pub struct Daemon {
    config: Config,
    broker: Arc<Broker>,
    supervisor: Rc<Supervisor>,
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
        let policy = Arc::new(DefaultPolicy::new(config.policy.external_requires));
        let mut builder = Broker::builder(policy, audit).guarantees(sandbox.guarantees());
        if config.demo.tools {
            for tool in demo::tools() {
                builder = builder.tool(tool);
            }
        }
        let broker = Arc::new(builder.build());
        let supervisor = Rc::new(Supervisor::new(broker.clone(), sandbox, config.task_dir()));
        Ok(Self {
            config,
            broker,
            supervisor,
        })
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
