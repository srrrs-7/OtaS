//! クライアント側（タスクや人間向けの画面が使う）。

use crate::IpcError;
use crate::convert::{label_to_wire, read_labels, role_to_wire, text};
use crate::otas_capnp::{
    approver, call_result, console, pending_approval, task_listener, task_session,
};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use otas_ports::{InferRequest, InferResponse, ModelCaps, ToolSpec};
use otas_types::{Action, ApprovalId, GrantSet, LabelSet, TaskId};
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// 操作の結果。
#[derive(Debug, Clone, PartialEq)]
pub enum Remote<T> {
    /// 実行が終わった。
    Done(T),
    /// 人間の承認を待っている。
    Pending(ApprovalId),
    /// 拒否された（理由つき）。
    Denied(String),
}

impl<T> Remote<T> {
    /// 承認待ちなら ID を返す。
    pub fn pending(&self) -> Option<ApprovalId> {
        match self {
            Remote::Pending(id) => Some(*id),
            _ => None,
        }
    }
}

fn parse_uuid(s: &str) -> Result<uuid::Uuid, IpcError> {
    uuid::Uuid::parse_str(s).map_err(|e| IpcError::Protocol(format!("ID が不正です: {e}")))
}

fn read_result(reader: call_result::Reader<'_>) -> Result<Remote<serde_json::Value>, IpcError> {
    match reader
        .which()
        .map_err(|e| IpcError::Protocol(format!("{e:?}")))?
    {
        call_result::Which::Done(t) => Ok(Remote::Done(serde_json::from_str(&text(t?)?)?)),
        call_result::Which::Pending(t) => Ok(Remote::Pending(ApprovalId(parse_uuid(&text(t?)?)?))),
        call_result::Which::Denied(t) => Ok(Remote::Denied(text(t?)?)),
    }
}

/// 接続の上で RPC を動かし、相手が公開した最初のケイパビリティを返す。
/// `LocalSet` の中で呼ぶ必要がある。
fn bootstrap<C: capnp::capability::FromClientHook>(stream: UnixStream) -> C {
    let (reader, writer) = stream.into_split();
    let network = twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader.compat()),
        futures::io::BufWriter::new(writer.compat_write()),
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc = RpcSystem::new(Box::new(network), None);
    let client = rpc.bootstrap(rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc);
    client
}

/// タスクの窓口に接続する。`LocalSet` の中で呼ぶ必要がある。
pub fn connect_task(stream: UnixStream) -> TaskClient {
    TaskClient {
        cap: bootstrap(stream),
    }
}

/// 承認の窓口に接続する。`LocalSet` の中で呼ぶ必要がある。
pub fn connect_approver(stream: UnixStream) -> ApproverClient {
    ApproverClient {
        cap: bootstrap(stream),
    }
}

/// 人間向けの窓口に接続する。`LocalSet` の中で呼ぶ必要がある。
pub fn connect_console(stream: UnixStream) -> ConsoleClient {
    ConsoleClient {
        cap: bootstrap(stream),
    }
}

/// 人間向けの窓口（タスクの依頼・一覧・承認）。
#[derive(Clone)]
pub struct ConsoleClient {
    cap: console::Client,
}

impl ConsoleClient {
    /// タスクを依頼する。
    pub async fn submit(&self, request_json: &str) -> Result<TaskId, IpcError> {
        let mut req = self.cap.submit_request();
        req.get().set_request_json(request_json);
        let response = req.send().promise.await?;
        Ok(TaskId(parse_uuid(&text(response.get()?.get_task_id()?)?)?))
    }

    /// タスクの一覧（ID と、状態の JSON）。
    pub async fn tasks(&self) -> Result<Vec<(TaskId, serde_json::Value)>, IpcError> {
        let response = self.cap.tasks_request().send().promise.await?;
        let mut items = Vec::new();
        for item in response.get()?.get_items()?.iter() {
            items.push((
                TaskId(parse_uuid(&text(item.get_id()?)?)?),
                serde_json::from_str(&text(item.get_status_json()?)?)?,
            ));
        }
        Ok(items)
    }

    /// 作業領域の変更。
    pub async fn review(&self, task: TaskId) -> Result<serde_json::Value, IpcError> {
        let mut req = self.cap.review_request();
        req.get().set_task_id(task.0.to_string());
        let response = req.send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_changes_json()?,
        )?)?)
    }

    /// 作業領域の変更を正本にコミットする。
    pub async fn commit(&self, task: TaskId) -> Result<serde_json::Value, IpcError> {
        let mut req = self.cap.commit_request();
        req.get().set_task_id(task.0.to_string());
        let response = req.send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_result_json()?,
        )?)?)
    }

    /// 作業領域を捨てる。
    pub async fn discard(&self, task: TaskId) -> Result<(), IpcError> {
        let mut req = self.cap.discard_request();
        req.get().set_task_id(task.0.to_string());
        req.send().promise.await?;
        Ok(())
    }

    /// 正本の版の一覧。
    pub async fn versions(&self) -> Result<serde_json::Value, IpcError> {
        let response = self.cap.versions_request().send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_versions_json()?,
        )?)?)
    }

    /// 正本を指定した版に戻す。
    pub async fn rollback(&self, version: u64) -> Result<serde_json::Value, IpcError> {
        let mut req = self.cap.rollback_request();
        req.get().set_version(version);
        let response = req.send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_result_json()?,
        )?)?)
    }

    /// タスクの経過を受け取り始める。最初にそれまでの出力と承認待ちが届き、
    /// 最後に [`WatchEvent::Finished`] が届く。承認待ちは2回届くことがある（ID で重複を除く）。
    /// `LocalSet` の中で呼ぶ必要がある。
    pub async fn watch(
        &self,
        task: TaskId,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<WatchEvent>, IpcError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener: task_listener::Client = capnp_rpc::new_client(Listener { tx });
        let mut req = self.cap.watch_request();
        req.get().set_task_id(task.0.to_string());
        req.get().set_listener(listener);
        req.send().promise.await?;
        Ok(rx)
    }

    /// 承認の窓口。
    pub fn approver(&self) -> ApproverClient {
        // パイプライン化：応答を待たずに、返ってくるはずの窓口を使い始められる
        let response = self.cap.approver_request().send();
        ApproverClient {
            cap: response.pipeline.get_approver(),
        }
    }
}

/// タスクの窓口。この窓口からの操作は、常に窓口に紐づいたタスクとして扱われる。
#[derive(Clone)]
pub struct TaskClient {
    cap: task_session::Client,
}

impl TaskClient {
    /// サンドボックスの中のエージェントが、標準入力として渡された窓口に接続する。
    /// `LocalSet` の中で呼ぶ必要がある。
    pub fn from_stdin() -> Result<TaskClient, IpcError> {
        use std::os::fd::AsFd;
        let fd = std::io::stdin().as_fd().try_clone_to_owned()?;
        let stream = std::os::unix::net::UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Ok(connect_task(UnixStream::from_std(stream)?))
    }

    /// ツールを呼ぶ。
    pub async fn call_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<Remote<serde_json::Value>, IpcError> {
        let mut req = self.cap.call_tool_request();
        req.get().set_name(name);
        req.get().set_args_json(serde_json::to_string(args)?);
        let response = req.send().promise.await?;
        read_result(response.get()?.get_result()?)
    }

    /// モデルで推論する。完了した場合は応答の本文を返す。
    pub async fn infer(
        &self,
        model: &str,
        request: &InferRequest,
    ) -> Result<Remote<String>, IpcError> {
        let mut req = self.cap.infer_request();
        {
            let mut p = req.get();
            p.set_model(model);
            p.set_max_tokens(request.max_tokens);
            let mut list = p.init_messages(request.messages.len() as u32);
            for (i, m) in request.messages.iter().enumerate() {
                let mut b = list.reborrow().get(i as u32);
                b.set_role(role_to_wire(m.role));
                b.set_content(&m.content);
            }
        }
        let response = req.send().promise.await?;
        Ok(match read_result(response.get()?.get_result()?)? {
            Remote::Done(v) => Remote::Done(
                v.as_str()
                    .ok_or_else(|| IpcError::Protocol("推論の結果が文字列ではありません".into()))?
                    .to_string(),
            ),
            Remote::Pending(id) => Remote::Pending(id),
            Remote::Denied(r) => Remote::Denied(r),
        })
    }

    /// モデルで推論する（ツールの呼び出しを含む）。
    pub async fn infer_full(
        &self,
        model: &str,
        request: &InferRequest,
    ) -> Result<Remote<InferResponse>, IpcError> {
        let mut req = self.cap.infer_full_request();
        req.get().set_model(model);
        req.get().set_request_json(serde_json::to_string(request)?);
        let response = req.send().promise.await?;
        Ok(match read_result(response.get()?.get_result()?)? {
            Remote::Done(v) => Remote::Done(serde_json::from_value(v)?),
            Remote::Pending(id) => Remote::Pending(id),
            Remote::Denied(r) => Remote::Denied(r),
        })
    }

    /// 承認待ちになった操作が決着するまで待ち、結果を受け取る。
    /// 結果は元の操作と同じ形の JSON（推論は `InferResponse`）。却下されたときは `Denied`。
    pub async fn wait_approval(
        &self,
        id: ApprovalId,
    ) -> Result<Remote<serde_json::Value>, IpcError> {
        let mut req = self.cap.wait_approval_request();
        req.get().set_id(id.0.to_string());
        let response = req.send().promise.await?;
        read_result(response.get()?.get_result()?)
    }

    /// このタスクが呼べるツールの一覧。
    pub async fn tools(&self) -> Result<Vec<ToolSpec>, IpcError> {
        let response = self.cap.tools_request().send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_tools_json()?,
        )?)?)
    }

    /// このタスクが使えるモデルの一覧。
    pub async fn models(&self) -> Result<Vec<ModelCaps>, IpcError> {
        let response = self.cap.models_request().send().promise.await?;
        Ok(serde_json::from_str(&text(
            response.get()?.get_models_json()?,
        )?)?)
    }

    /// データの読み取りを判定する。
    pub async fn read_data(&self, path: &str, labels: &LabelSet) -> Result<Remote<()>, IpcError> {
        let mut req = self.cap.read_data_request();
        {
            let mut p = req.get();
            p.set_path(path);
            let mut list = p.init_labels(labels.iter().count() as u32);
            for (i, l) in labels.iter().enumerate() {
                list.set(i as u32, label_to_wire(l));
            }
        }
        let response = req.send().promise.await?;
        Ok(match read_result(response.get()?.get_result()?)? {
            Remote::Done(_) => Remote::Done(()),
            Remote::Pending(id) => Remote::Pending(id),
            Remote::Denied(r) => Remote::Denied(r),
        })
    }

    /// タスクが積み上げたラベル。
    pub async fn labels(&self) -> Result<LabelSet, IpcError> {
        let response = self.cap.labels_request().send().promise.await?;
        Ok(read_labels(response.get()?.get_labels()?)?)
    }

    /// 子タスクを作り、その窓口を返す。権限は親の範囲内に限られる。
    pub async fn delegate(&self, grants: &GrantSet) -> Result<TaskClient, IpcError> {
        let mut req = self.cap.delegate_request();
        req.get().set_grants_json(serde_json::to_string(grants)?);
        let response = req.send().promise.await?;
        Ok(TaskClient {
            cap: response.get()?.get_child()?,
        })
    }

    /// 子タスクのラベルを取り込む。
    pub async fn absorb(&self, child: &TaskClient) -> Result<(), IpcError> {
        let mut req = self.cap.absorb_request();
        req.get().set_child(child.cap.clone());
        req.send().promise.await?;
        Ok(())
    }
}

/// タスクの経過の知らせ（[`ConsoleClient::watch`]）。
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    /// エージェントの標準出力（完全な行だけ）。
    Output(String),
    /// 承認待ちができた。
    Approval(PendingItem),
    /// タスクが終わった（状態）。これが最後の知らせ。
    Finished(serde_json::Value),
}

/// デーモンからの知らせを受け取り、チャネルに流す。
struct Listener {
    tx: tokio::sync::mpsc::UnboundedSender<WatchEvent>,
}

impl Listener {
    fn forward(&self, event: WatchEvent) -> Result<(), capnp::Error> {
        // 受け取る側がもういなければ、デーモンに伝えて送るのをやめてもらう
        self.tx
            .send(event)
            .map_err(|_| capnp::Error::disconnected("受け取る側が終わりました".into()))
    }
}

impl task_listener::Server for Listener {
    async fn output(
        self: capnp::capability::Rc<Self>,
        params: task_listener::OutputParams,
        _results: task_listener::OutputResults,
    ) -> Result<(), capnp::Error> {
        let lines = text(params.get()?.get_lines()?).map_err(protocol)?;
        self.forward(WatchEvent::Output(lines))
    }

    async fn approval(
        self: capnp::capability::Rc<Self>,
        params: task_listener::ApprovalParams,
        _results: task_listener::ApprovalResults,
    ) -> Result<(), capnp::Error> {
        let item = read_pending(params.get()?.get_item()?).map_err(protocol)?;
        self.forward(WatchEvent::Approval(item))
    }

    async fn finished(
        self: capnp::capability::Rc<Self>,
        params: task_listener::FinishedParams,
        _results: task_listener::FinishedResults,
    ) -> Result<(), capnp::Error> {
        let status = text(params.get()?.get_status_json()?).map_err(protocol)?;
        let status = serde_json::from_str(&status).map_err(protocol)?;
        self.forward(WatchEvent::Finished(status))
    }
}

fn protocol(e: impl std::fmt::Display) -> capnp::Error {
    capnp::Error::failed(e.to_string())
}

fn read_pending(item: pending_approval::Reader<'_>) -> Result<PendingItem, IpcError> {
    Ok(PendingItem {
        id: ApprovalId(parse_uuid(&text(item.get_id()?)?)?),
        task: TaskId(parse_uuid(&text(item.get_task()?)?)?),
        action: serde_json::from_str(&text(item.get_action()?)?)?,
        reason: text(item.get_reason()?)?,
    })
}

/// 承認待ちの1件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingItem {
    /// 承認待ちの ID。
    pub id: ApprovalId,
    /// 要求したタスク。
    pub task: TaskId,
    /// 要求された操作。
    pub action: Action,
    /// 承認が必要な理由。
    pub reason: String,
}

/// 承認の窓口。人間向けの画面だけが持つ。
#[derive(Clone)]
pub struct ApproverClient {
    cap: approver::Client,
}

impl ApproverClient {
    /// 承認待ちの一覧。
    pub async fn pending(&self) -> Result<Vec<PendingItem>, IpcError> {
        let response = self.cap.pending_request().send().promise.await?;
        let mut items = Vec::new();
        for item in response.get()?.get_items()?.iter() {
            items.push(read_pending(item)?);
        }
        Ok(items)
    }

    /// 承認して実行する。完了した場合は結果（JSON）を返す。
    pub async fn approve(&self, id: ApprovalId) -> Result<Remote<serde_json::Value>, IpcError> {
        let mut req = self.cap.approve_request();
        req.get().set_id(id.0.to_string());
        let response = req.send().promise.await?;
        read_result(response.get()?.get_result()?)
    }

    /// 却下する。
    pub async fn reject(&self, id: ApprovalId) -> Result<(), IpcError> {
        let mut req = self.cap.reject_request();
        req.get().set_id(id.0.to_string());
        req.send().promise.await?;
        Ok(())
    }
}
