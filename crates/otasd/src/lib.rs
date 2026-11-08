//! OtaS のデーモン（`otasd`）。
//!
//! 人間の依頼からタスクを作り、エージェントのプロセスをサンドボックスに入れて起動し、
//! そのタスク専用の窓口を標準入力として渡す。人間向けの窓口（依頼・一覧・承認）は
//! Unix ソケットで公開し、接続元が同じユーザーであることを OS に確かめさせる。
//!
//! ```text
//! 人間 ── otas（CLI）── console.sock ──▶ otasd ─┬─ ブローカー（判定・記録）
//!                                                 └─ 監督 ── サンドボックス ── エージェント
//!                                                              （stdin がタスク専用の窓口）
//! ```

#![cfg(target_os = "linux")]

pub mod config;
pub mod daemon;
pub mod demo;
pub mod supervisor;

pub use config::Config;
pub use daemon::{Daemon, DaemonError, sandbox_from_config};
pub use supervisor::{Supervisor, SupervisorError, TaskRequest, TaskStatus};
