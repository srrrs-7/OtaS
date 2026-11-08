//! OtaS のブローカー。
//!
//! タスクが外の世界に作用する手段は、ブローカーを通る呼び出しだけにする。
//! 呼び出しごとに次の順で処理し、どのアダプタやプラグインを差し替えてもこの順序は変えない。
//!
//! 1. タスクの権限とラベルを取り出す
//! 2. ポリシーで判定する（許可・拒否・人間の承認待ち）
//! 3. 実行する**前に**監査ログに記録する（記録できなければ実行しない）
//! 4. 実行し、結果のラベルをタスクに取り込む
//! 5. 結果を監査ログに記録する

use otas_ports::{
    AuditError, AuditSink, InferRequest, InferResponse, ModelError, ModelProvider, PolicyContext,
    PolicyEngine, ToolError, ToolOutput, ToolProvider,
};
use otas_types::{
    AccessRequest, Action, ApprovalId, AttenuationError, AuditEntry, Decision, Effect, GrantSet,
    Guarantees, LabelSet, Outcome, TaskId,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// ブローカーのエラー。
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    /// 登録されていないタスク。
    #[error("タスク {0} は登録されていません")]
    UnknownTask(TaskId),
    /// 登録されていないツール。
    #[error("ツール {0} は登録されていません")]
    UnknownTool(String),
    /// 登録されていないモデル。
    #[error("モデル {0} は登録されていません")]
    UnknownModel(String),
    /// 存在しない承認待ち。
    #[error("承認待ち {0} は存在しません")]
    UnknownApproval(ApprovalId),
    /// 子タスクが親タスクの子ではない。
    #[error("タスク {child} はタスク {parent} の子ではありません")]
    NotAChild {
        /// 親として指定したタスク。
        parent: TaskId,
        /// 子として指定したタスク。
        child: TaskId,
    },
    /// ポリシーに拒否された。
    #[error("拒否されました: {0}")]
    Denied(String),
    /// 委任できない権限を要求した。
    #[error(transparent)]
    Attenuation(#[from] AttenuationError),
    /// ツールの実行に失敗した。
    #[error(transparent)]
    Tool(#[from] ToolError),
    /// 推論に失敗した。
    #[error(transparent)]
    Model(#[from] ModelError),
    /// 監査ログに書けなかった。
    #[error(transparent)]
    Audit(#[from] AuditError),
}

/// 呼び出しの結果。
#[derive(Debug, Clone, PartialEq)]
pub enum CallResult<T> {
    /// 実行が終わった。
    Done(T),
    /// 人間の承認を待っている。
    Pending(ApprovalId),
}

impl<T> CallResult<T> {
    /// 実行済みなら結果を返す。
    pub fn done(self) -> Option<T> {
        match self {
            CallResult::Done(v) => Some(v),
            CallResult::Pending(_) => None,
        }
    }

    /// 承認待ちなら ID を返す。
    pub fn pending(&self) -> Option<ApprovalId> {
        match self {
            CallResult::Done(_) => None,
            CallResult::Pending(id) => Some(*id),
        }
    }
}

/// 承認されて実行された操作の結果。
#[derive(Debug, Clone, PartialEq)]
pub enum Completed {
    /// データの読み取りが許可された。
    Read,
    /// データの書き込み（コミット）が許可された。
    Write,
    /// ツールの実行結果。
    Tool(ToolOutput),
    /// 推論の結果。
    Infer(InferResponse),
}

/// 人間に見せる承認待ちの一覧の1件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// 承認待ちの ID。
    pub id: ApprovalId,
    /// 要求したタスク。
    pub task: TaskId,
    /// 要求された操作。
    pub action: Action,
    /// 承認が必要な理由。
    pub reason: String,
}

#[derive(Debug)]
struct TaskEntry {
    grants: GrantSet,
    labels: LabelSet,
    parent: Option<TaskId>,
}

#[derive(Debug)]
enum PendingOp {
    Read { data_labels: LabelSet },
    Write,
    Tool { args: serde_json::Value },
    Infer { request: InferRequest },
}

#[derive(Debug)]
struct PendingCall {
    task: TaskId,
    action: Action,
    effect: Effect,
    reason: String,
    op: PendingOp,
}

#[derive(Debug, Default)]
struct State {
    tasks: HashMap<TaskId, TaskEntry>,
    pending: HashMap<ApprovalId, PendingCall>,
}

enum Gate {
    Proceed {
        labels: LabelSet,
        decision: Decision,
    },
    Held(ApprovalId),
}

/// [`Broker`] を組み立てる。
pub struct BrokerBuilder {
    policy: Arc<dyn PolicyEngine>,
    audit: Arc<dyn AuditSink>,
    guarantees: Guarantees,
    tools: HashMap<String, Arc<dyn ToolProvider>>,
    models: HashMap<String, Arc<dyn ModelProvider>>,
}

impl BrokerBuilder {
    /// 実行環境の保証を設定する（既定は保証なし）。
    pub fn guarantees(mut self, guarantees: Guarantees) -> Self {
        self.guarantees = guarantees;
        self
    }

    /// ツールを登録する。同じ名前のツールは後から登録したもので置き換わる。
    pub fn tool(mut self, tool: Arc<dyn ToolProvider>) -> Self {
        self.tools.insert(tool.spec().name, tool);
        self
    }

    /// モデルを登録する。同じ ID のモデルは後から登録したもので置き換わる。
    pub fn model(mut self, model: Arc<dyn ModelProvider>) -> Self {
        self.models.insert(model.caps().id, model);
        self
    }

    /// ブローカーを作る。
    pub fn build(self) -> Broker {
        Broker {
            policy: self.policy,
            audit: self.audit,
            guarantees: self.guarantees,
            tools: self.tools,
            models: self.models,
            state: Mutex::new(State::default()),
        }
    }
}

/// タスクの操作をすべて仲介する。
pub struct Broker {
    policy: Arc<dyn PolicyEngine>,
    audit: Arc<dyn AuditSink>,
    guarantees: Guarantees,
    tools: HashMap<String, Arc<dyn ToolProvider>>,
    models: HashMap<String, Arc<dyn ModelProvider>>,
    state: Mutex<State>,
}

impl Broker {
    /// ポリシーと監査ログを指定して組み立てを始める。
    pub fn builder(policy: Arc<dyn PolicyEngine>, audit: Arc<dyn AuditSink>) -> BrokerBuilder {
        BrokerBuilder {
            policy,
            audit,
            guarantees: Guarantees::NONE,
            tools: HashMap::new(),
            models: HashMap::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // タスク
    // -----------------------------------------------------------------------

    /// 最上位のタスクを登録する（エージェント監督が人間の依頼から作る）。
    pub fn spawn_task(&self, grants: GrantSet) -> TaskId {
        let id = TaskId::new();
        self.state().tasks.insert(
            id,
            TaskEntry {
                grants,
                labels: LabelSet::empty(),
                parent: None,
            },
        );
        id
    }

    /// 子タスクを作る。子の権限は親の権限の範囲内に限られ、親のラベルを引き継ぐ。
    pub fn delegate(&self, parent: TaskId, grants: GrantSet) -> Result<TaskId, BrokerError> {
        let mut state = self.state();
        let parent_entry = state
            .tasks
            .get(&parent)
            .ok_or(BrokerError::UnknownTask(parent))?;
        let grants = parent_entry.grants.attenuate(grants)?;
        let labels = parent_entry.labels.clone();
        let id = TaskId::new();
        state.tasks.insert(
            id,
            TaskEntry {
                grants,
                labels,
                parent: Some(parent),
            },
        );
        Ok(id)
    }

    /// 子タスクの結果を親が受け取るときに呼ぶ。子が積み上げたラベルを親に取り込む。
    pub fn absorb_child(&self, parent: TaskId, child: TaskId) -> Result<(), BrokerError> {
        let mut state = self.state();
        let child_entry = state
            .tasks
            .get(&child)
            .ok_or(BrokerError::UnknownTask(child))?;
        if child_entry.parent != Some(parent) {
            return Err(BrokerError::NotAChild { parent, child });
        }
        let child_labels = child_entry.labels.clone();
        let parent_entry = state
            .tasks
            .get_mut(&parent)
            .ok_or(BrokerError::UnknownTask(parent))?;
        otas_ifc::propagate(&mut parent_entry.labels, &child_labels);
        Ok(())
    }

    /// タスクが積み上げたラベルを返す。
    pub fn labels(&self, task: TaskId) -> Result<LabelSet, BrokerError> {
        self.state()
            .tasks
            .get(&task)
            .map(|t| t.labels.clone())
            .ok_or(BrokerError::UnknownTask(task))
    }

    // -----------------------------------------------------------------------
    // 操作
    // -----------------------------------------------------------------------

    /// データの読み取りを判定する。許可されたら、データのラベルをタスクに取り込む。
    ///
    /// 実際の読み取りは作業領域（次の段階で実装）が行う。ここでは判定と記録とラベル伝播だけを行う。
    pub fn read_data(
        &self,
        task: TaskId,
        path: &str,
        data_labels: &LabelSet,
    ) -> Result<CallResult<()>, BrokerError> {
        let action = Action::ReadData { path: path.into() };
        match self.gate(task, &action, Effect::Internal, || PendingOp::Read {
            data_labels: data_labels.clone(),
        })? {
            Gate::Held(id) => Ok(CallResult::Pending(id)),
            Gate::Proceed { labels, decision } => {
                self.authorize(task, &action, Effect::Internal, &labels, &decision, false)?;
                self.observe(task, data_labels);
                self.record(
                    task,
                    &action,
                    Effect::Internal,
                    labels,
                    decision,
                    false,
                    Outcome::Executed,
                )?;
                Ok(CallResult::Done(()))
            }
        }
    }

    /// データの書き込み（作業領域の変更を正本にコミットすること）を判定する。
    ///
    /// 実際の書き込みは作業領域が行う。ここでは判定と記録だけを行う。
    pub fn write_data(&self, task: TaskId, path: &str) -> Result<CallResult<()>, BrokerError> {
        let action = Action::WriteData { path: path.into() };
        match self.gate(task, &action, Effect::Internal, || PendingOp::Write)? {
            Gate::Held(id) => Ok(CallResult::Pending(id)),
            Gate::Proceed { labels, decision } => {
                self.authorize(task, &action, Effect::Internal, &labels, &decision, false)?;
                self.record(
                    task,
                    &action,
                    Effect::Internal,
                    labels,
                    decision,
                    false,
                    Outcome::Executed,
                )?;
                Ok(CallResult::Done(()))
            }
        }
    }

    /// ツールを呼び出す。
    pub async fn call_tool(
        &self,
        task: TaskId,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallResult<ToolOutput>, BrokerError> {
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| BrokerError::UnknownTool(name.into()))?;
        let effect = tool.spec().effect;
        let action = Action::CallTool { tool: name.into() };
        match self.gate(task, &action, effect, || PendingOp::Tool {
            args: args.clone(),
        })? {
            Gate::Held(id) => Ok(CallResult::Pending(id)),
            Gate::Proceed { labels, decision } => self
                .run_tool(task, tool, args, labels, decision, false)
                .await
                .map(CallResult::Done),
        }
    }

    /// モデルで推論する。クラウドのモデルへの入力は外部への作用として判定する。
    pub async fn infer(
        &self,
        task: TaskId,
        model: &str,
        request: InferRequest,
    ) -> Result<CallResult<InferResponse>, BrokerError> {
        let provider = self
            .models
            .get(model)
            .cloned()
            .ok_or_else(|| BrokerError::UnknownModel(model.into()))?;
        let effect = provider.caps().effect();
        let action = Action::Infer {
            model: model.into(),
        };
        match self.gate(task, &action, effect, || PendingOp::Infer {
            request: request.clone(),
        })? {
            Gate::Held(id) => Ok(CallResult::Pending(id)),
            Gate::Proceed { labels, decision } => self
                .run_infer(task, provider, request, labels, decision, false)
                .await
                .map(CallResult::Done),
        }
    }

    // -----------------------------------------------------------------------
    // 承認
    // -----------------------------------------------------------------------

    /// 承認待ちの一覧を返す。
    pub fn pending(&self) -> Vec<PendingApproval> {
        self.state()
            .pending
            .iter()
            .map(|(id, p)| PendingApproval {
                id: *id,
                task: p.task,
                action: p.action.clone(),
                reason: p.reason.clone(),
            })
            .collect()
    }

    /// 人間が承認した操作を実行する。
    ///
    /// 承認後にもう一度ポリシーで判定し、その間に権限が取り消されていれば拒否する。
    /// 承認で覆せるのは「人間の承認待ち」という判定だけで、拒否は覆せない。
    pub async fn approve(&self, id: ApprovalId) -> Result<Completed, BrokerError> {
        let pending = self
            .state()
            .pending
            .remove(&id)
            .ok_or(BrokerError::UnknownApproval(id))?;
        let PendingCall {
            task,
            action,
            effect,
            reason,
            op,
        } = pending;

        let (decision, labels) = self.judge(task, &action, effect)?;
        if let Decision::Deny { reason } = decision {
            self.record(
                task,
                &action,
                effect,
                labels,
                Decision::Deny {
                    reason: reason.clone(),
                },
                true,
                Outcome::Denied,
            )?;
            return Err(BrokerError::Denied(reason));
        }
        let decision = Decision::AskHuman { reason };

        match op {
            PendingOp::Read { data_labels } => {
                self.authorize(task, &action, effect, &labels, &decision, true)?;
                self.observe(task, &data_labels);
                self.record(
                    task,
                    &action,
                    effect,
                    labels,
                    decision,
                    true,
                    Outcome::Executed,
                )?;
                Ok(Completed::Read)
            }
            PendingOp::Write => {
                self.authorize(task, &action, effect, &labels, &decision, true)?;
                self.record(
                    task,
                    &action,
                    effect,
                    labels,
                    decision,
                    true,
                    Outcome::Executed,
                )?;
                Ok(Completed::Write)
            }
            PendingOp::Tool { args } => {
                let name = match &action {
                    Action::CallTool { tool } => tool.clone(),
                    other => unreachable!("ツールの承認待ちに別の操作が入っている: {other}"),
                };
                let tool = self
                    .tools
                    .get(&name)
                    .cloned()
                    .ok_or(BrokerError::UnknownTool(name))?;
                self.run_tool(task, tool, args, labels, decision, true)
                    .await
                    .map(Completed::Tool)
            }
            PendingOp::Infer { request } => {
                let model = match &action {
                    Action::Infer { model } => model.clone(),
                    other => unreachable!("推論の承認待ちに別の操作が入っている: {other}"),
                };
                let provider = self
                    .models
                    .get(&model)
                    .cloned()
                    .ok_or(BrokerError::UnknownModel(model))?;
                self.run_infer(task, provider, request, labels, decision, true)
                    .await
                    .map(Completed::Infer)
            }
        }
    }

    /// 人間が却下した操作を破棄する。操作は実行されない。
    pub fn reject(&self, id: ApprovalId) -> Result<(), BrokerError> {
        let pending = self
            .state()
            .pending
            .remove(&id)
            .ok_or(BrokerError::UnknownApproval(id))?;
        let labels = self.labels(pending.task)?;
        self.record(
            pending.task,
            &pending.action,
            pending.effect,
            labels,
            Decision::AskHuman {
                reason: pending.reason,
            },
            false,
            Outcome::Rejected { approval: id },
        )
    }

    // -----------------------------------------------------------------------
    // 内部処理
    // -----------------------------------------------------------------------

    fn judge(
        &self,
        task: TaskId,
        action: &Action,
        effect: Effect,
    ) -> Result<(Decision, LabelSet), BrokerError> {
        let (grants, labels) = {
            let state = self.state();
            let entry = state
                .tasks
                .get(&task)
                .ok_or(BrokerError::UnknownTask(task))?;
            (entry.grants.clone(), entry.labels.clone())
        };
        let request = AccessRequest {
            task,
            action: action.clone(),
            effect,
        };
        let decision = self.policy.decide(&PolicyContext {
            request: &request,
            grants: &grants,
            labels: &labels,
            guarantees: &self.guarantees,
        });
        Ok((decision, labels))
    }

    fn gate(
        &self,
        task: TaskId,
        action: &Action,
        effect: Effect,
        op: impl FnOnce() -> PendingOp,
    ) -> Result<Gate, BrokerError> {
        let (decision, labels) = self.judge(task, action, effect)?;
        match decision {
            Decision::Allow => Ok(Gate::Proceed { labels, decision }),
            Decision::Deny { reason } => {
                self.record(
                    task,
                    action,
                    effect,
                    labels,
                    Decision::Deny {
                        reason: reason.clone(),
                    },
                    false,
                    Outcome::Denied,
                )?;
                Err(BrokerError::Denied(reason))
            }
            Decision::AskHuman { reason } => {
                let id = ApprovalId::new();
                // 先に記録し、記録できなかったら承認待ちにも入れない
                self.record(
                    task,
                    action,
                    effect,
                    labels,
                    Decision::AskHuman {
                        reason: reason.clone(),
                    },
                    false,
                    Outcome::Pending { approval: id },
                )?;
                self.state().pending.insert(
                    id,
                    PendingCall {
                        task,
                        action: action.clone(),
                        effect,
                        reason,
                        op: op(),
                    },
                );
                Ok(Gate::Held(id))
            }
        }
    }

    async fn run_tool(
        &self,
        task: TaskId,
        tool: Arc<dyn ToolProvider>,
        args: serde_json::Value,
        labels: LabelSet,
        decision: Decision,
        approved: bool,
    ) -> Result<ToolOutput, BrokerError> {
        let spec = tool.spec();
        let action = Action::CallTool {
            tool: spec.name.clone(),
        };
        self.authorize(task, &action, spec.effect, &labels, &decision, approved)?;
        match tool.call(args).await {
            Ok(output) => {
                self.observe(task, &spec.output_labels.joined(&output.labels));
                self.record(
                    task,
                    &action,
                    spec.effect,
                    labels,
                    decision,
                    approved,
                    Outcome::Executed,
                )?;
                Ok(output)
            }
            Err(e) => {
                let outcome = Outcome::Failed {
                    error: e.to_string(),
                };
                self.record(
                    task,
                    &action,
                    spec.effect,
                    labels,
                    decision,
                    approved,
                    outcome,
                )?;
                Err(e.into())
            }
        }
    }

    async fn run_infer(
        &self,
        task: TaskId,
        provider: Arc<dyn ModelProvider>,
        request: InferRequest,
        labels: LabelSet,
        decision: Decision,
        approved: bool,
    ) -> Result<InferResponse, BrokerError> {
        let caps = provider.caps();
        let action = Action::Infer { model: caps.id };
        let effect = if caps.local {
            Effect::Internal
        } else {
            Effect::External
        };
        self.authorize(task, &action, effect, &labels, &decision, approved)?;
        match provider.infer(request).await {
            Ok(response) => {
                // 応答はタスクの文脈から作られるので、ラベルはすでにタスクが持っている
                self.record(
                    task,
                    &action,
                    effect,
                    labels,
                    decision,
                    approved,
                    Outcome::Executed,
                )?;
                Ok(response)
            }
            Err(e) => {
                let outcome = Outcome::Failed {
                    error: e.to_string(),
                };
                self.record(task, &action, effect, labels, decision, approved, outcome)?;
                Err(e.into())
            }
        }
    }

    /// 実行前の記録。書けなければ実行しない。
    fn authorize(
        &self,
        task: TaskId,
        action: &Action,
        effect: Effect,
        labels: &LabelSet,
        decision: &Decision,
        approved: bool,
    ) -> Result<(), BrokerError> {
        self.record(
            task,
            action,
            effect,
            labels.clone(),
            decision.clone(),
            approved,
            Outcome::Authorized,
        )
    }

    fn observe(&self, task: TaskId, observed: &LabelSet) {
        if let Some(entry) = self.state().tasks.get_mut(&task) {
            otas_ifc::propagate(&mut entry.labels, observed);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        task: TaskId,
        action: &Action,
        effect: Effect,
        labels: LabelSet,
        decision: Decision,
        approved_by_human: bool,
        outcome: Outcome,
    ) -> Result<(), BrokerError> {
        self.audit.append(AuditEntry {
            task,
            action: action.clone(),
            effect,
            labels,
            decision,
            approved_by_human,
            outcome,
        })?;
        Ok(())
    }
}
