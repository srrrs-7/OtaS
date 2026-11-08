//! サーバ側（ブローカーを公開する側）。

use crate::convert::{read_labels, role_from_wire, text, write_labels};
use crate::otas_capnp::{approver, call_result, console, task_session};
use capnp::capability::Rc;
use capnp_rpc::{CapabilityServerSet, RpcSystem, rpc_twoparty_capnp, twoparty};
use otas_broker::{Broker, BrokerError, CallResult, Completed};
use otas_ports::{InferRequest, Message};
use otas_types::{ApprovalId, GrantSet, TaskId};
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

type SessionSet = CapabilityServerSet<TaskSessionImpl, task_session::Client>;

/// サーバが発行した `TaskSession` の台帳。
///
/// 相手から `TaskSession` を受け取ったとき（子のラベルを取り込むときなど）、
/// それが本当にこのサーバの発行したものかを、この台帳で確かめる。
#[derive(Clone)]
pub struct Sessions {
    broker: Arc<Broker>,
    set: std::rc::Rc<RefCell<SessionSet>>,
}

impl Sessions {
    /// ブローカーを公開する台帳を作る。
    pub fn new(broker: Arc<Broker>) -> Self {
        Self {
            broker,
            set: std::rc::Rc::new(RefCell::new(CapabilityServerSet::new())),
        }
    }

    /// タスクの窓口を発行する。
    pub fn session(&self, task: TaskId) -> task_session::Client {
        let server = TaskSessionImpl {
            task,
            sessions: self.clone(),
        };
        self.set.borrow_mut().new_client(server)
    }
}

struct TaskSessionImpl {
    task: TaskId,
    sessions: Sessions,
}

impl TaskSessionImpl {
    fn broker(&self) -> &Broker {
        &self.sessions.broker
    }
}

fn failed(e: impl std::fmt::Display) -> capnp::Error {
    capnp::Error::failed(e.to_string())
}

/// ブローカーの結果をスキーマの結果に書き込む。拒否はエラーではなく結果として返す。
fn write_result<T>(
    mut builder: call_result::Builder<'_>,
    outcome: Result<CallResult<T>, BrokerError>,
    to_json: impl FnOnce(T) -> Result<String, serde_json::Error>,
) -> capnp::Result<()> {
    match outcome {
        Ok(CallResult::Done(value)) => builder.set_done(to_json(value).map_err(failed)?),
        Ok(CallResult::Pending(id)) => builder.set_pending(id.0.to_string()),
        Err(BrokerError::Denied(reason)) => builder.set_denied(reason),
        Err(other) => return Err(failed(other)),
    }
    Ok(())
}

impl task_session::Server for TaskSessionImpl {
    async fn call_tool(
        self: Rc<Self>,
        params: task_session::CallToolParams,
        mut results: task_session::CallToolResults,
    ) -> Result<(), capnp::Error> {
        let (name, args) = {
            let p = params.get()?;
            (text(p.get_name()?)?, text(p.get_args_json()?)?)
        };
        let args: serde_json::Value = serde_json::from_str(&args).map_err(failed)?;
        let outcome = self.broker().call_tool(self.task, &name, args).await;
        write_result(results.get().init_result(), outcome, |out| {
            serde_json::to_string(&out.content)
        })
    }

    async fn infer(
        self: Rc<Self>,
        params: task_session::InferParams,
        mut results: task_session::InferResults,
    ) -> Result<(), capnp::Error> {
        let (model, request) = {
            let p = params.get()?;
            let mut messages = Vec::new();
            for m in p.get_messages()?.iter() {
                let role = m
                    .get_role()
                    .map_err(|e| failed(format!("未知の役割です: {e:?}")))?;
                messages.push(Message {
                    role: role_from_wire(role),
                    content: text(m.get_content()?)?,
                });
            }
            let request = InferRequest {
                messages,
                max_tokens: p.get_max_tokens(),
            };
            (text(p.get_model()?)?, request)
        };
        let outcome = self.broker().infer(self.task, &model, request).await;
        write_result(results.get().init_result(), outcome, |r| {
            serde_json::to_string(&r.content)
        })
    }

    async fn read_data(
        self: Rc<Self>,
        params: task_session::ReadDataParams,
        mut results: task_session::ReadDataResults,
    ) -> Result<(), capnp::Error> {
        let (path, labels) = {
            let p = params.get()?;
            (text(p.get_path()?)?, read_labels(p.get_labels()?)?)
        };
        let outcome = self.broker().read_data(self.task, &path, &labels);
        write_result(results.get().init_result(), outcome, |()| {
            Ok("null".to_string())
        })
    }

    async fn labels(
        self: Rc<Self>,
        _params: task_session::LabelsParams,
        mut results: task_session::LabelsResults,
    ) -> Result<(), capnp::Error> {
        let labels = self.broker().labels(self.task).map_err(failed)?;
        let n = labels.iter().count() as u32;
        write_labels(results.get().init_labels(n), &labels);
        Ok(())
    }

    async fn delegate(
        self: Rc<Self>,
        params: task_session::DelegateParams,
        mut results: task_session::DelegateResults,
    ) -> Result<(), capnp::Error> {
        let grants = text(params.get()?.get_grants_json()?)?;
        let grants: GrantSet = serde_json::from_str(&grants).map_err(failed)?;
        let child = self.broker().delegate(self.task, grants).map_err(failed)?;
        results.get().set_child(self.sessions.session(child));
        Ok(())
    }

    async fn absorb(
        self: Rc<Self>,
        params: task_session::AbsorbParams,
        _results: task_session::AbsorbResults,
    ) -> Result<(), capnp::Error> {
        // このサーバが発行した窓口が返ってきた場合、受信した時点でサーバ内の実体に置き換わっている。
        // そうでない窓口（別の接続から持ち込まれたものなど）は解決を待たずに拒否する。
        // 解決を待つと、相手しだいで応答が返らなくなることがある。
        let child = params.get()?.get_child()?;
        let child_task = self
            .sessions
            .set
            .borrow()
            .get_local_server_of_resolved(&child)
            .map(|s| s.task)
            .ok_or_else(|| failed("このサーバが発行した窓口ではありません"))?;
        self.broker()
            .absorb_child(self.task, child_task)
            .map_err(failed)
    }
}

struct ApproverImpl {
    broker: Arc<Broker>,
}

fn parse_approval(id: &str) -> capnp::Result<ApprovalId> {
    uuid::Uuid::parse_str(id)
        .map(ApprovalId)
        .map_err(|e| failed(format!("承認待ちの ID が不正です: {e}")))
}

impl approver::Server for ApproverImpl {
    async fn pending(
        self: Rc<Self>,
        _params: approver::PendingParams,
        mut results: approver::PendingResults,
    ) -> Result<(), capnp::Error> {
        let items = self.broker.pending();
        let mut list = results.get().init_items(items.len() as u32);
        for (i, item) in items.iter().enumerate() {
            let mut b = list.reborrow().get(i as u32);
            b.set_id(item.id.0.to_string());
            b.set_task(item.task.0.to_string());
            b.set_action(serde_json::to_string(&item.action).map_err(failed)?);
            b.set_reason(&item.reason);
        }
        Ok(())
    }

    async fn approve(
        self: Rc<Self>,
        params: approver::ApproveParams,
        mut results: approver::ApproveResults,
    ) -> Result<(), capnp::Error> {
        let id = parse_approval(&text(params.get()?.get_id()?)?)?;
        let outcome = self.broker.approve(id).await.map(CallResult::Done);
        write_result(results.get().init_result(), outcome, |done| match done {
            Completed::Read => Ok("null".to_string()),
            Completed::Tool(out) => serde_json::to_string(&out.content),
            Completed::Infer(r) => serde_json::to_string(&r.content),
        })
    }

    async fn reject(
        self: Rc<Self>,
        params: approver::RejectParams,
        _results: approver::RejectResults,
    ) -> Result<(), capnp::Error> {
        let id = parse_approval(&text(params.get()?.get_id()?)?)?;
        self.broker.reject(id).map_err(failed)
    }
}

fn rpc_system(
    stream: UnixStream,
    bootstrap: capnp::capability::Client,
) -> RpcSystem<rpc_twoparty_capnp::Side> {
    let (reader, writer) = stream.into_split();
    let network = twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader.compat()),
        futures::io::BufWriter::new(writer.compat_write()),
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    RpcSystem::new(Box::new(network), Some(bootstrap))
}

/// タスク専用の窓口を、接続の最初の（唯一の）ケイパビリティとして公開する。
///
/// 返す Future は接続が切れるまで動き続けるので、`LocalSet` の中で `spawn_local` する。
/// 接続元の確認（[`crate::verify_peer_uid`]）は、呼び出す前に済ませておく。
pub fn serve_task(
    stream: UnixStream,
    sessions: &Sessions,
    task: TaskId,
) -> impl Future<Output = Result<(), capnp::Error>> + 'static {
    let client = sessions.session(task);
    rpc_system(stream, client.client)
}

/// 承認の窓口を公開する。人間向けの画面からの接続にだけ使う。
pub fn serve_approver(
    stream: UnixStream,
    broker: Arc<Broker>,
) -> impl Future<Output = Result<(), capnp::Error>> + 'static {
    let client: approver::Client = capnp_rpc::new_client(ApproverImpl { broker });
    rpc_system(stream, client.client)
}

/// 人間向けの窓口の中身。タスクの依頼と一覧はデーモン（監督）が実装する。
pub trait ConsoleBackend {
    /// タスクを依頼する。
    fn submit(&self, request_json: &str) -> Result<TaskId, String>;
    /// タスクの一覧（ID と、状態の JSON）。
    fn tasks(&self) -> Vec<(TaskId, String)>;
}

struct ConsoleImpl {
    backend: std::rc::Rc<dyn ConsoleBackend>,
    broker: Arc<Broker>,
}

impl console::Server for ConsoleImpl {
    async fn submit(
        self: Rc<Self>,
        params: console::SubmitParams,
        mut results: console::SubmitResults,
    ) -> Result<(), capnp::Error> {
        let request = text(params.get()?.get_request_json()?)?;
        let task = self.backend.submit(&request).map_err(failed)?;
        results.get().set_task_id(task.0.to_string());
        Ok(())
    }

    async fn tasks(
        self: Rc<Self>,
        _params: console::TasksParams,
        mut results: console::TasksResults,
    ) -> Result<(), capnp::Error> {
        let tasks = self.backend.tasks();
        let mut list = results.get().init_items(tasks.len() as u32);
        for (i, (id, status)) in tasks.iter().enumerate() {
            let mut b = list.reborrow().get(i as u32);
            b.set_id(id.0.to_string());
            b.set_status_json(status);
        }
        Ok(())
    }

    async fn approver(
        self: Rc<Self>,
        _params: console::ApproverParams,
        mut results: console::ApproverResults,
    ) -> Result<(), capnp::Error> {
        let approver: approver::Client = capnp_rpc::new_client(ApproverImpl {
            broker: self.broker.clone(),
        });
        results.get().set_approver(approver);
        Ok(())
    }
}

/// 人間向けの窓口を公開する。接続元の確認（[`crate::verify_peer_uid`]）は、呼び出す前に済ませておく。
pub fn serve_console(
    stream: UnixStream,
    backend: std::rc::Rc<dyn ConsoleBackend>,
    broker: Arc<Broker>,
) -> impl Future<Output = Result<(), capnp::Error>> + 'static {
    let client: console::Client = capnp_rpc::new_client(ConsoleImpl { backend, broker });
    rpc_system(stream, client.client)
}
