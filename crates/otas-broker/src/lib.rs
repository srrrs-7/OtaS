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
    AuditError, AuditSink, InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider,
    PolicyContext, PolicyEngine, StateError, StateStore, ToolError, ToolOutput, ToolProvider,
    ToolSpec,
};
use otas_types::{
    AccessRequest, Action, ApprovalId, AttenuationError, AuditEntry, Decision, Effect, GrantSet,
    Guarantees, LabelSet, Outcome, TaskId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

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
    /// 状態を保存または読み込みできなかった。
    #[error(transparent)]
    State(#[from] StateError),
}

/// 状態を保存するときのキー。
pub const STATE_KEY: &str = "broker";

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

/// 承認待ちの操作がどう決着したか。タスクが [`Broker::poll_approval`] で受け取る。
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// 承認されて実行された。
    Done(Completed),
    /// 承認後の再判定で拒否された。
    Denied(String),
    /// 人間が却下した。
    Rejected,
    /// 承認されたが、実行に失敗した。
    Failed(String),
}

/// 承認待ちの操作の、いまの状態。
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalState {
    /// まだ決着していない（人間の判断を待っている、または承認後に実行している）。
    Waiting,
    /// 決着した。結果は1回だけ受け取れる。
    Resolved(Resolution),
}

/// 承認待ちができたときに呼ばれる関数。ブローカーのロックの外で呼ぶ。
pub type PendingListener = Arc<dyn Fn(&PendingApproval) + Send + Sync>;

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

#[derive(Debug, Serialize, Deserialize)]
struct TaskEntry {
    grants: GrantSet,
    labels: LabelSet,
    parent: Option<TaskId>,
}

#[derive(Debug, Serialize, Deserialize)]
enum PendingOp {
    Read { data_labels: LabelSet },
    Write,
    Tool { args: serde_json::Value },
    Infer { request: InferRequest },
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingCall {
    task: TaskId,
    action: Action,
    effect: Effect,
    reason: String,
    op: PendingOp,
}

#[derive(Debug, Default, Serialize, Deserialize)]
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
    store: Option<Arc<dyn StateStore>>,
}

impl BrokerBuilder {
    /// 状態の保存先を設定する。設定すると、状態が変わるたびに保存する。
    /// 保存した状態を読み戻すには、作った後に [`Broker::restore`] を呼ぶ。
    pub fn state(mut self, store: Arc<dyn StateStore>) -> Self {
        self.store = Some(store);
        self
    }

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
            resolutions: Mutex::new(HashMap::new()),
            on_pending: OnceLock::new(),
            store: self.store,
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
    /// 承認待ちから外した操作の決着。値が `None` の間は実行中。
    /// タスクが受け取るまで残す（保存はしない。デーモンが止まればエージェントも止まるため）。
    /// ロックの順序は `state` → `resolutions`。
    resolutions: Mutex<HashMap<ApprovalId, (TaskId, Option<Resolution>)>>,
    on_pending: OnceLock<PendingListener>,
    store: Option<Arc<dyn StateStore>>,
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
            store: None,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 状態を丸ごと保存する（保存先がなければ何もしない）。ロックを持ったまま呼ぶ。
    fn persist(&self, state: &State) -> Result<(), BrokerError> {
        if let Some(store) = &self.store {
            let bytes = serde_json::to_vec(state)
                .map_err(|e| StateError::Corrupt(format!("状態を書き出せません: {e}")))?;
            store.save(STATE_KEY, &bytes)?;
        }
        Ok(())
    }

    /// 保存した状態を読み戻す（タスクの権限とラベル、承認待ち）。デーモンの起動時に1回呼ぶ。
    ///
    /// 読み戻したタスクのエージェントはもう動いていない（デーモンと一緒に止まっている）。
    /// 承認待ちの操作と、作業領域のコミットの判定のために、権限とラベルを残しておく。
    pub fn restore(&self) -> Result<(), BrokerError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if let Some(bytes) = store.load(STATE_KEY)? {
            let restored: State = serde_json::from_slice(&bytes)
                .map_err(|e| StateError::Corrupt(format!("ブローカーの状態を読めません: {e}")))?;
            *self.state() = restored;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // タスク
    // -----------------------------------------------------------------------

    /// 最上位のタスクを登録する（エージェント監督が人間の依頼から作る）。
    ///
    /// 新しいタスクの保存に失敗しても登録は続ける。ラベルのない新しいタスクが再起動で消えても、
    /// 後の操作が「不明なタスク」として拒否されるだけで安全側に倒れる。また、次に状態が変わったときに
    /// 状態を丸ごと保存し直すので、そこで回復する。
    pub fn spawn_task(&self, grants: GrantSet) -> TaskId {
        let id = TaskId::new();
        let mut state = self.state();
        state.tasks.insert(
            id,
            TaskEntry {
                grants,
                labels: LabelSet::empty(),
                parent: None,
            },
        );
        let _ = self.persist(&state);
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
        self.persist(&state)?;
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
        self.persist(&state)?;
        Ok(())
    }

    /// 前のタスクのラベルを引き継ぐ。会話の続きのように、前のタスクの結果を入力にするときに使う。
    ///
    /// 前のタスクが触れた私的データや信頼できない入力は、結果の文章を通じて次のタスクに入る。
    /// ラベルを引き継がないと、その文章がラベルのないデータとして扱われてしまう（ラベルの洗浄）。
    /// ラベルは増える方向にしか変わらないので、引き継いで権限が広がることはない。
    pub fn inherit_labels(&self, task: TaskId, from: TaskId) -> Result<(), BrokerError> {
        let mut state = self.state();
        let labels = state
            .tasks
            .get(&from)
            .ok_or(BrokerError::UnknownTask(from))?
            .labels
            .clone();
        let entry = state
            .tasks
            .get_mut(&task)
            .ok_or(BrokerError::UnknownTask(task))?;
        otas_ifc::propagate(&mut entry.labels, &labels);
        self.persist(&state)?;
        Ok(())
    }

    /// タスクが呼べるツールの一覧（権限のあるものだけ。名前順）。エージェントがモデルに見せるために使う。
    pub fn tools_for(&self, task: TaskId) -> Result<Vec<ToolSpec>, BrokerError> {
        let grants = self.grants_of(task)?;
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|t| t.spec())
            .filter(|s| {
                grants.permits(&Action::CallTool {
                    tool: s.name.clone(),
                })
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs)
    }

    /// タスクが使えるモデルの一覧（権限のあるものだけ。ID 順）。
    pub fn models_for(&self, task: TaskId) -> Result<Vec<ModelCaps>, BrokerError> {
        let grants = self.grants_of(task)?;
        let mut caps: Vec<ModelCaps> = self
            .models
            .values()
            .map(|m| m.caps())
            .filter(|c| {
                grants.permits(&Action::Infer {
                    model: c.id.clone(),
                })
            })
            .collect();
        caps.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(caps)
    }

    fn grants_of(&self, task: TaskId) -> Result<GrantSet, BrokerError> {
        self.state()
            .tasks
            .get(&task)
            .map(|t| t.grants.clone())
            .ok_or(BrokerError::UnknownTask(task))
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
                self.observe(task, data_labels)?;
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

    /// 承認待ちができたときに呼ぶ関数を設定する（1回だけ。2回目以降は無視する）。
    /// 人間向けの画面に、承認待ちをすぐに知らせるために使う。
    pub fn set_pending_listener(&self, listener: PendingListener) {
        let _ = self.on_pending.set(listener);
    }

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
        // 実行する前に承認待ちから外して保存する。途中で落ちても、同じ操作を2回実行しない（多くて1回）
        let pending = self.take_pending(id)?;
        let outcome = self.execute_approved(pending).await;
        self.resolve(
            id,
            match &outcome {
                Ok(done) => Resolution::Done(done.clone()),
                Err(BrokerError::Denied(reason)) => Resolution::Denied(reason.clone()),
                Err(e) => Resolution::Failed(e.to_string()),
            },
        );
        outcome
    }

    async fn execute_approved(&self, pending: PendingCall) -> Result<Completed, BrokerError> {
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
                self.observe(task, &data_labels)?;
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
        let pending = self.take_pending(id)?;
        self.resolve(id, Resolution::Rejected);
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

    /// タスクの承認待ちの操作が決着したかを確かめる。決着していれば結果を返し、記録から消す。
    ///
    /// 承認待ちのエージェントが、人間の判断を待って続きを進めるために使う。
    /// 他のタスクの承認待ちの ID は、存在しないものとして扱う。
    pub fn poll_approval(
        &self,
        task: TaskId,
        id: ApprovalId,
    ) -> Result<ApprovalState, BrokerError> {
        let state = self.state();
        if state.pending.get(&id).is_some_and(|p| p.task == task) {
            return Ok(ApprovalState::Waiting);
        }
        let mut resolutions = self.resolutions();
        match resolutions.get(&id) {
            Some((owner, None)) if *owner == task => Ok(ApprovalState::Waiting),
            Some((owner, Some(_))) if *owner == task => {
                let (_, resolution) = resolutions.remove(&id).expect("直前に確かめた");
                Ok(ApprovalState::Resolved(resolution.expect("直前に確かめた")))
            }
            _ => Err(BrokerError::UnknownApproval(id)),
        }
    }

    // -----------------------------------------------------------------------
    // 内部処理
    // -----------------------------------------------------------------------

    fn resolutions(&self) -> MutexGuard<'_, HashMap<ApprovalId, (TaskId, Option<Resolution>)>> {
        self.resolutions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn resolve(&self, id: ApprovalId, resolution: Resolution) {
        if let Some(entry) = self.resolutions().get_mut(&id) {
            entry.1 = Some(resolution);
        }
    }

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
                let mut state = self.state();
                state.pending.insert(
                    id,
                    PendingCall {
                        task,
                        action: action.clone(),
                        effect,
                        reason: reason.clone(),
                        op: op(),
                    },
                );
                self.persist(&state)?;
                drop(state);
                if let Some(listener) = self.on_pending.get() {
                    listener(&PendingApproval {
                        id,
                        task,
                        action: action.clone(),
                        reason,
                    });
                }
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
                self.observe(task, &spec.output_labels.joined(&output.labels))?;
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

    /// 観測したラベルをタスクに取り込み、保存する。ラベルは安全に関わるので、保存の失敗はエラーにする。
    fn observe(&self, task: TaskId, observed: &LabelSet) -> Result<(), BrokerError> {
        let mut state = self.state();
        if let Some(entry) = state.tasks.get_mut(&task) {
            otas_ifc::propagate(&mut entry.labels, observed);
        }
        self.persist(&state)
    }

    fn take_pending(&self, id: ApprovalId) -> Result<PendingCall, BrokerError> {
        let mut state = self.state();
        let pending = state
            .pending
            .remove(&id)
            .ok_or(BrokerError::UnknownApproval(id))?;
        self.persist(&state)?;
        // `state` のロックを持ったまま「実行中」として記録する（待っているタスクが見失わないように）
        self.resolutions().insert(id, (pending.task, None));
        Ok(pending)
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
