//! ブローカーの結合テスト。標準ポリシーと監査ログを組み合わせ、偽のツールとモデルで動かす。

use async_trait::async_trait;
use otas_audit::{MemoryAuditLog, verify_chain};
use otas_broker::{ApprovalState, Broker, BrokerError, CallResult, Completed, Resolution};
use otas_policy::DefaultPolicy;
use otas_ports::{
    InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider, Role, ToolError, ToolOutput,
    ToolProvider, ToolSpec,
};
use otas_types::{Effect, Grant, GrantSet, Guarantees, Label, LabelSet, Outcome};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// 偽のツールとモデル
// ---------------------------------------------------------------------------

struct FakeTool {
    spec: ToolSpec,
    calls: AtomicUsize,
}

impl FakeTool {
    fn new(name: &str, effect: Effect, output_labels: LabelSet) -> Arc<Self> {
        Arc::new(Self {
            spec: ToolSpec {
                name: name.into(),
                description: format!("テスト用のツール {name}"),
                effect,
                output_labels,
                input_schema: json!({"type": "object"}),
            },
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolProvider for FakeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({ "tool": self.spec.name, "args": args }),
            labels: LabelSet::empty(),
        })
    }
}

struct FakeModel {
    id: String,
    local: bool,
}

#[async_trait]
impl ModelProvider for FakeModel {
    fn caps(&self) -> ModelCaps {
        ModelCaps {
            id: self.id.clone(),
            local: self.local,
            context_window: 8192,
            tool_use: true,
        }
    }

    async fn infer(&self, request: InferRequest) -> Result<InferResponse, ModelError> {
        Ok(InferResponse {
            content: format!(
                "{} が {} 件のメッセージに応答",
                self.id,
                request.messages.len()
            ),
            ..InferResponse::default()
        })
    }
}

// ---------------------------------------------------------------------------
// 組み立て
// ---------------------------------------------------------------------------

struct World {
    broker: Broker,
    audit: Arc<MemoryAuditLog>,
    mail_read: Arc<FakeTool>,
    mail_send: Arc<FakeTool>,
    web_fetch: Arc<FakeTool>,
}

fn world_with(guarantees: Guarantees) -> World {
    let audit = Arc::new(MemoryAuditLog::new());
    // 私的データを返す読み取り
    let mail_read = FakeTool::new(
        "mail.read",
        Effect::Internal,
        LabelSet::of([Label::Private]),
    );
    // 外部への送信
    let mail_send = FakeTool::new("mail.send", Effect::External, LabelSet::empty());
    // Web の取得。要求自体が外部に出て、結果は信頼できない
    let web_fetch = FakeTool::new(
        "web.fetch",
        Effect::External,
        LabelSet::of([Label::Untrusted]),
    );

    let broker = Broker::builder(Arc::new(DefaultPolicy::default()), audit.clone())
        .guarantees(guarantees)
        .tool(mail_read.clone())
        .tool(mail_send.clone())
        .tool(web_fetch.clone())
        .model(Arc::new(FakeModel {
            id: "local-small".into(),
            local: true,
        }))
        .model(Arc::new(FakeModel {
            id: "cloud-large".into(),
            local: false,
        }))
        .build();

    World {
        broker,
        audit,
        mail_read,
        mail_send,
        web_fetch,
    }
}

fn world() -> World {
    world_with(Guarantees::STRONG)
}

fn all_tools() -> GrantSet {
    GrantSet::new([
        Grant::Tool {
            pattern: "*".into(),
        },
        Grant::Model {
            pattern: "*".into(),
        },
        Grant::Read {
            prefix: "/data".into(),
        },
    ])
}

fn hello() -> InferRequest {
    InferRequest {
        messages: vec![otas_ports::Message::text(Role::User, "こんにちは")],
        max_tokens: 64,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reading_mail_is_allowed_and_its_label_flows_into_the_task() {
    let w = world();
    let task = w.broker.spawn_task(GrantSet::new([Grant::Tool {
        pattern: "mail.*".into(),
    }]));

    let result = w
        .broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    assert!(matches!(result, CallResult::Done(_)));
    assert!(w.broker.labels(task).unwrap().contains(Label::Private));
    assert_eq!(w.mail_read.calls(), 1);
}

#[tokio::test]
async fn a_call_without_a_grant_is_denied_and_recorded() {
    let w = world();
    let task = w.broker.spawn_task(GrantSet::none());

    let err = w
        .broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, BrokerError::Denied(_)));
    assert_eq!(w.mail_read.calls(), 0);

    let records = w.audit.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].entry.outcome, Outcome::Denied);
}

#[tokio::test]
async fn rule_of_two_holds_an_external_send_until_a_human_approves() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());

    // 私的データを読む（条件 2 つ目）
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    // Web を取得する。この時点では信頼できない入力にまだ触れていないので通る
    let fetched = w
        .broker
        .call_tool(task, "web.fetch", json!({ "url": "https://example.com" }))
        .await
        .unwrap();
    assert!(matches!(fetched, CallResult::Done(_)));

    // 3 つの条件がそろったので、送信は保留される
    let held = w
        .broker
        .call_tool(task, "mail.send", json!({ "to": "someone@example.com" }))
        .await
        .unwrap();
    let id = held.pending().expect("承認待ちになるはず");
    assert_eq!(w.mail_send.calls(), 0);
    assert_eq!(w.broker.pending().len(), 1);

    // 人間が承認すると実行される
    let completed = w.broker.approve(id).await.unwrap();
    assert!(matches!(completed, Completed::Tool(_)));
    assert_eq!(w.mail_send.calls(), 1);
    assert!(w.broker.pending().is_empty());

    let records = w.audit.records();
    verify_chain(&records).unwrap();
    let last = records.last().unwrap();
    assert!(last.entry.approved_by_human);
    assert_eq!(last.entry.outcome, Outcome::Executed);
}

#[tokio::test]
async fn a_rejected_call_never_runs() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    w.broker
        .call_tool(task, "web.fetch", json!({}))
        .await
        .unwrap();

    let id = w
        .broker
        .call_tool(task, "mail.send", json!({}))
        .await
        .unwrap()
        .pending()
        .unwrap();
    w.broker.reject(id).unwrap();

    assert_eq!(w.mail_send.calls(), 0);
    assert!(matches!(
        w.broker.approve(id).await,
        Err(BrokerError::UnknownApproval(_))
    ));
    assert!(matches!(
        w.audit.records().last().unwrap().entry.outcome,
        Outcome::Rejected { .. }
    ));
}

/// 承認待ちのタスクは、人間の判断を待ち、決着した結果を1回だけ受け取れる。他のタスクには見えない。
#[tokio::test]
async fn a_waiting_task_receives_the_outcome_of_its_approval_once() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());
    let other = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    w.broker
        .call_tool(task, "web.fetch", json!({}))
        .await
        .unwrap();
    let send = |to: &'static str| {
        let broker = &w.broker;
        async move {
            broker
                .call_tool(task, "mail.send", json!({ "to": to }))
                .await
                .unwrap()
                .pending()
                .unwrap()
        }
    };

    // 承認：待っている間は Waiting、承認すると実行結果を受け取る
    let approved = send("a@example.com").await;
    assert_eq!(
        w.broker.poll_approval(task, approved).unwrap(),
        ApprovalState::Waiting
    );
    assert!(matches!(
        w.broker.poll_approval(other, approved),
        Err(BrokerError::UnknownApproval(_))
    ));
    w.broker.approve(approved).await.unwrap();
    match w.broker.poll_approval(task, approved).unwrap() {
        ApprovalState::Resolved(Resolution::Done(Completed::Tool(out))) => {
            assert_eq!(out.content["args"]["to"], "a@example.com");
        }
        other => panic!("実行結果を受け取るはず: {other:?}"),
    }
    // 受け取るのは1回だけ
    assert!(matches!(
        w.broker.poll_approval(task, approved),
        Err(BrokerError::UnknownApproval(_))
    ));

    // 却下：実行されず、却下されたことを受け取る
    let rejected = send("b@example.com").await;
    w.broker.reject(rejected).unwrap();
    assert_eq!(
        w.broker.poll_approval(task, rejected).unwrap(),
        ApprovalState::Resolved(Resolution::Rejected)
    );
    assert_eq!(w.mail_send.calls(), 1);
}

/// 承認待ちができると、設定した関数ですぐに知らせる。
#[tokio::test]
async fn a_new_approval_is_announced_to_the_listener() {
    let w = world();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    w.broker
        .set_pending_listener(Arc::new(move |p| log.lock().unwrap().push(p.clone())));
    let task = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    w.broker
        .call_tool(task, "web.fetch", json!({}))
        .await
        .unwrap();
    assert!(seen.lock().unwrap().is_empty());

    let id = w
        .broker
        .call_tool(task, "mail.send", json!({}))
        .await
        .unwrap()
        .pending()
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, w.broker.pending());
    assert_eq!(seen[0].id, id);
    assert_eq!(seen[0].task, task);
}

/// 会話の続き：前のタスクのラベルを引き継ぎ、前の結果を通じた洗浄を防ぐ。
#[tokio::test]
async fn a_follow_up_task_inherits_the_labels_of_the_previous_one() {
    let w = world();
    let first = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(first, "mail.read", json!({}))
        .await
        .unwrap();
    w.broker
        .call_tool(first, "web.fetch", json!({}))
        .await
        .unwrap();

    let next = w.broker.spawn_task(all_tools());
    assert!(w.broker.labels(next).unwrap().is_empty());
    w.broker.inherit_labels(next, first).unwrap();
    assert_eq!(
        w.broker.labels(next).unwrap(),
        w.broker.labels(first).unwrap()
    );
    // 引き継いだので、次のタスクでも外部への送信は承認待ちになる
    let held = w
        .broker
        .call_tool(next, "mail.send", json!({}))
        .await
        .unwrap();
    assert!(held.pending().is_some());

    let unknown = otas_types::TaskId::new();
    assert!(matches!(
        w.broker.inherit_labels(next, unknown),
        Err(BrokerError::UnknownTask(_))
    ));
}

#[tokio::test]
async fn a_child_cannot_use_a_grant_it_was_not_given() {
    let w = world();
    let parent = w.broker.spawn_task(all_tools());
    // 子には mail.send の権限を渡さない
    let child = w
        .broker
        .delegate(
            parent,
            GrantSet::new([Grant::Tool {
                pattern: "mail.read".into(),
            }]),
        )
        .unwrap();
    let err = w
        .broker
        .call_tool(child, "mail.send", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, BrokerError::Denied(_)));
}

#[tokio::test]
async fn secrets_never_leave_even_with_other_grants() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());

    w.broker
        .read_data(task, "/data/keys/api.key", &LabelSet::of([Label::Secret]))
        .unwrap();
    let err = w
        .broker
        .call_tool(task, "web.fetch", json!({ "url": "https://example.com" }))
        .await
        .unwrap_err();
    assert!(matches!(err, BrokerError::Denied(_)));
    assert_eq!(w.web_fetch.calls(), 0);
}

#[tokio::test]
async fn a_cloud_model_is_held_when_private_and_untrusted_data_meet_but_a_local_one_is_not() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    w.broker
        .call_tool(task, "web.fetch", json!({}))
        .await
        .unwrap();

    let local = w.broker.infer(task, "local-small", hello()).await.unwrap();
    assert!(matches!(local, CallResult::Done(_)));

    let cloud = w.broker.infer(task, "cloud-large", hello()).await.unwrap();
    let id = cloud
        .pending()
        .expect("クラウドへの送信は承認待ちになるはず");
    assert!(matches!(
        w.broker.approve(id).await.unwrap(),
        Completed::Infer(_)
    ));
}

#[tokio::test]
async fn delegation_only_narrows_grants_and_carries_labels_both_ways() {
    let w = world();
    let parent = w.broker.spawn_task(GrantSet::new([Grant::Tool {
        pattern: "mail.*".into(),
    }]));
    w.broker
        .call_tool(parent, "mail.read", json!({}))
        .await
        .unwrap();

    // 親より広い権限は渡せない
    let wider = w.broker.delegate(
        parent,
        GrantSet::new([Grant::Tool {
            pattern: "*".into(),
        }]),
    );
    assert!(matches!(wider, Err(BrokerError::Attenuation(_))));

    // 子は親のラベルを引き継ぐ
    let child = w
        .broker
        .delegate(
            parent,
            GrantSet::new([Grant::Tool {
                pattern: "mail.read".into(),
            }]),
        )
        .unwrap();
    assert!(w.broker.labels(child).unwrap().contains(Label::Private));

    // 子には読み取り権限を渡していないので、読み取りは拒否される
    let denied = w.broker.read_data(
        child,
        "/data/web/page.html",
        &LabelSet::of([Label::Untrusted]),
    );
    assert!(matches!(denied, Err(BrokerError::Denied(_))));

    // 親子関係のないタスクには、子のラベルを取り込めない
    let unrelated = w.broker.spawn_task(GrantSet::none());
    assert!(matches!(
        w.broker.absorb_child(unrelated, child),
        Err(BrokerError::NotAChild { .. })
    ));
    w.broker.absorb_child(parent, child).unwrap();
}

#[tokio::test]
async fn a_weak_environment_blocks_external_effects() {
    let w = world_with(Guarantees::NONE);
    let task = w.broker.spawn_task(all_tools());

    // 内部で完結する操作は通る
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();
    // 外部に作用する操作は、環境の保証が足りないので拒否される
    let err = w
        .broker
        .call_tool(task, "web.fetch", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, BrokerError::Denied(_)));
}

#[tokio::test]
async fn every_executed_call_is_recorded_before_it_runs() {
    let w = world();
    let task = w.broker.spawn_task(all_tools());
    w.broker
        .call_tool(task, "mail.read", json!({}))
        .await
        .unwrap();

    let outcomes: Vec<_> = w
        .audit
        .records()
        .into_iter()
        .map(|r| r.entry.outcome)
        .collect();
    assert_eq!(outcomes, vec![Outcome::Authorized, Outcome::Executed]);
}

/// 同じ保存先を使うブローカーを作る（再起動の代わり）。
fn persistent_broker(store: Arc<otas_state::MemoryStateStore>) -> (Broker, Arc<FakeTool>) {
    let mail_read = FakeTool::new(
        "mail.read",
        Effect::Internal,
        LabelSet::of([Label::Private]),
    );
    let mail_send = FakeTool::new("mail.send", Effect::External, LabelSet::empty());
    let web_fetch = FakeTool::new(
        "web.fetch",
        Effect::External,
        LabelSet::of([Label::Untrusted]),
    );
    let broker = Broker::builder(
        Arc::new(DefaultPolicy::default()),
        Arc::new(MemoryAuditLog::new()),
    )
    .guarantees(Guarantees::STRONG)
    .tool(mail_read)
    .tool(mail_send.clone())
    .tool(web_fetch)
    .state(store)
    .build();
    broker.restore().unwrap();
    (broker, mail_send)
}

#[tokio::test]
async fn labels_and_pending_approvals_survive_a_restart() {
    let store = Arc::new(otas_state::MemoryStateStore::new());

    let (task, approval) = {
        let (broker, mail_send) = persistent_broker(store.clone());
        let task = broker.spawn_task(all_tools());
        broker
            .call_tool(task, "mail.read", json!({}))
            .await
            .unwrap();
        broker
            .call_tool(task, "web.fetch", json!({}))
            .await
            .unwrap();
        let approval = broker
            .call_tool(task, "mail.send", json!({"to": "a@example.com"}))
            .await
            .unwrap()
            .pending()
            .unwrap();
        assert_eq!(mail_send.calls(), 0);
        (task, approval)
    };

    // 再起動後：ラベルと承認待ちが残っている
    let (broker, mail_send) = persistent_broker(store.clone());
    let labels = broker.labels(task).unwrap();
    assert!(labels.contains(Label::Private) && labels.contains(Label::Untrusted));
    let pending = broker.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, approval);

    // 承認すれば実行され、承認待ちから消えたことも保存される
    let done = broker.approve(approval).await.unwrap();
    assert!(matches!(done, Completed::Tool(_)));
    assert_eq!(mail_send.calls(), 1);

    let (broker, mail_send) = persistent_broker(store);
    assert!(broker.pending().is_empty());
    // 同じ承認をもう一度使って、2回実行することはできない
    assert!(matches!(
        broker.approve(approval).await,
        Err(BrokerError::UnknownApproval(_))
    ));
    assert_eq!(mail_send.calls(), 0);
}
