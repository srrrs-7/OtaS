//! OtaS のテスト用部品。
//!
//! 偽のツールとモデル、それらを登録した標準構成のブローカーを提供する。
//! 本番のコードからは使わない（`publish = false`）。

use async_trait::async_trait;
use otas_audit::MemoryAuditLog;
use otas_broker::Broker;
use otas_policy::DefaultPolicy;
use otas_ports::{
    InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider, ToolError, ToolOutput,
    ToolProvider, ToolSpec,
};
use otas_types::{Effect, Grant, GrantSet, Guarantees, Label, LabelSet};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 呼ばれた回数を数え、引数をそのまま返す偽のツール。
#[derive(Debug)]
pub struct FakeTool {
    spec: ToolSpec,
    calls: AtomicUsize,
}

impl FakeTool {
    /// 偽のツールを作る。
    pub fn new(name: &str, effect: Effect, output_labels: LabelSet) -> Arc<Self> {
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

    /// 呼ばれた回数。
    pub fn calls(&self) -> usize {
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

/// 決まった形の応答を返す偽のモデル。
#[derive(Debug)]
pub struct FakeModel {
    id: String,
    local: bool,
}

impl FakeModel {
    /// 偽のモデルを作る。`local` が `false` ならクラウドのモデルとして振る舞う。
    pub fn new(id: &str, local: bool) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            local,
        })
    }
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

/// 標準構成のブローカーと、その部品。
pub struct World {
    /// ブローカー。
    pub broker: Arc<Broker>,
    /// 監査ログ。
    pub audit: Arc<MemoryAuditLog>,
    /// 私的データを返す読み取り（内部で完結）。
    pub mail_read: Arc<FakeTool>,
    /// 外部への送信。
    pub mail_send: Arc<FakeTool>,
    /// Web の取得（要求は外部に出て、結果は信頼できない）。
    pub web_fetch: Arc<FakeTool>,
}

impl World {
    /// 指定した環境の保証で組み立てる。
    /// モデルは `local-small`（ローカル）と `cloud-large`（クラウド）を登録する。
    pub fn new(guarantees: Guarantees) -> Self {
        let audit = Arc::new(MemoryAuditLog::new());
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

        let broker = Broker::builder(Arc::new(DefaultPolicy::default()), audit.clone())
            .guarantees(guarantees)
            .tool(mail_read.clone())
            .tool(mail_send.clone())
            .tool(web_fetch.clone())
            .model(FakeModel::new("local-small", true))
            .model(FakeModel::new("cloud-large", false))
            .build();

        Self {
            broker: Arc::new(broker),
            audit,
            mail_read,
            mail_send,
            web_fetch,
        }
    }

    /// 強い保証の環境（Linux 本体を想定）で組み立てる。
    pub fn strong() -> Self {
        Self::new(Guarantees::STRONG)
    }
}

/// すべてのツールとモデル、`/data` 以下の読み取りを許す権限。
pub fn all_grants() -> GrantSet {
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
