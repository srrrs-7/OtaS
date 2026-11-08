//! OtaS の IPC 層（Cap'n Proto RPC）。
//!
//! 権限をケイパビリティ（偽造できない参照）として渡すのがこの層の要点。
//!
//! - サンドボックス内のタスクには、そのタスク専用の `TaskSession` だけを渡す
//!   （[`serve_task`]）。操作は常にそのタスクとして扱われ、他のタスクを名乗る手段はない。
//! - 子タスクへの委任は、権限を絞った新しい `TaskSession` として返る。
//! - 承認の操作（`Approver`）は、人間向けの画面にだけ渡す（[`serve_approver`]）。
//! - 接続を受け付ける前に、接続元のプロセスのユーザー ID を OS に確認させる（[`verify_peer_uid`]）。
//!
//! capnp-rpc は1スレッドで動く前提なので、サーバもクライアントも tokio の `LocalSet` の中で動かす。
//! 現在は Unix ソケットだけに対応する（Windows の名前付きパイプは次の段階）。

#![cfg(unix)]

#[allow(missing_docs, clippy::all, clippy::pedantic)]
pub mod otas_capnp {
    include!(concat!(env!("OUT_DIR"), "/otas_capnp.rs"));
}

mod client;
mod convert;
mod server;

pub use client::{
    ApproverClient, ConsoleClient, PendingItem, Remote, TaskClient, connect_approver,
    connect_console, connect_task,
};
pub use server::{ConsoleBackend, Sessions, serve_approver, serve_console, serve_task};

use tokio::net::UnixStream;

/// IPC 層のエラー。
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// RPC がエラーを返した、または接続が切れた。
    #[error("RPC に失敗しました: {0}")]
    Rpc(#[from] capnp::Error),
    /// JSON の変換に失敗した。
    #[error("JSON の変換に失敗しました: {0}")]
    Json(#[from] serde_json::Error),
    /// 相手が仕様と違う値を送ってきた。
    #[error("プロトコル違反です: {0}")]
    Protocol(String),
    /// 接続元が許可されたユーザーではない。
    #[error("接続元のユーザー（uid {actual}）は許可されていません（期待した uid {expected}）")]
    PeerRejected {
        /// 期待したユーザー ID。
        expected: u32,
        /// 実際のユーザー ID。
        actual: u32,
    },
    /// ソケットの操作に失敗した。
    #[error("ソケットの操作に失敗しました: {0}")]
    Io(#[from] std::io::Error),
}

/// 接続元のプロセスのユーザー ID が `expected` と一致するか、OS に確認させる。
pub fn verify_peer_uid(stream: &UnixStream, expected: u32) -> Result<(), IpcError> {
    let actual = stream.peer_cred()?.uid();
    if actual == expected {
        Ok(())
    } else {
        Err(IpcError::PeerRejected { expected, actual })
    }
}
