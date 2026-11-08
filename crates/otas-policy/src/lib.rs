//! OtaS の標準ポリシーエンジン。
//!
//! 次の順に判定し、最初に引っかかったところで結論を出す。
//!
//! 1. 実行環境の保証が足りなければ、外部に作用する操作を拒否する
//! 2. タスクに権限がなければ拒否する
//! 3. 情報フロー制御（`otas-ifc`）の判定に従う
//!
//! 宣言的なポリシー言語（Cedar など）によるエンジンは、別の実装として後から追加する。

use otas_ports::{PolicyContext, PolicyEngine};
use otas_types::{Decision, Effect, Guarantees};

/// 標準ポリシー。
#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    min_guarantees: Guarantees,
}

impl DefaultPolicy {
    /// 外部に作用する操作に必要な、実行環境の最低限の保証を指定して作る。
    pub fn new(min_guarantees: Guarantees) -> Self {
        Self { min_guarantees }
    }
}

impl Default for DefaultPolicy {
    /// 本体（Linux）と同じ強い保証を要求する。
    fn default() -> Self {
        Self::new(Guarantees::STRONG)
    }
}

impl PolicyEngine for DefaultPolicy {
    fn decide(&self, ctx: &PolicyContext<'_>) -> Decision {
        let request = ctx.request;

        if request.effect == Effect::External && !ctx.guarantees.satisfies(&self.min_guarantees) {
            return Decision::Deny {
                reason: "この実行環境の隔離の保証では、外部に作用する操作を許可できません".into(),
            };
        }

        if !ctx.grants.permits(&request.action) {
            return Decision::Deny {
                reason: format!("このタスクには「{}」の権限がありません", request.action),
            };
        }

        match otas_ifc::evaluate(ctx.labels, request.effect) {
            otas_ifc::Verdict::Ok => Decision::Allow,
            otas_ifc::Verdict::RequiresApproval { reason } => Decision::AskHuman { reason },
            otas_ifc::Verdict::Forbidden { reason } => Decision::Deny { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otas_types::{AccessRequest, Action, Grant, GrantSet, Label, LabelSet, TaskId};

    fn decide(
        policy: &DefaultPolicy,
        effect: Effect,
        grants: &GrantSet,
        labels: &LabelSet,
        env: &Guarantees,
    ) -> Decision {
        let request = AccessRequest {
            task: TaskId::new(),
            action: Action::CallTool {
                tool: "mail.send".into(),
            },
            effect,
        };
        policy.decide(&PolicyContext {
            request: &request,
            grants,
            labels,
            guarantees: env,
        })
    }

    fn mail_grants() -> GrantSet {
        GrantSet::new([Grant::Tool {
            pattern: "mail.*".into(),
        }])
    }

    #[test]
    fn allows_granted_action() {
        let d = decide(
            &DefaultPolicy::default(),
            Effect::External,
            &mail_grants(),
            &LabelSet::empty(),
            &Guarantees::STRONG,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn denies_without_grant() {
        let d = decide(
            &DefaultPolicy::default(),
            Effect::Internal,
            &GrantSet::none(),
            &LabelSet::empty(),
            &Guarantees::STRONG,
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn weak_environment_blocks_external_effects_only() {
        let policy = DefaultPolicy::default();
        let grants = mail_grants();
        let labels = LabelSet::empty();
        assert!(matches!(
            decide(
                &policy,
                Effect::External,
                &grants,
                &labels,
                &Guarantees::NONE
            ),
            Decision::Deny { .. }
        ));
        assert_eq!(
            decide(
                &policy,
                Effect::Internal,
                &grants,
                &labels,
                &Guarantees::NONE
            ),
            Decision::Allow
        );
    }

    #[test]
    fn rule_of_two_asks_human() {
        let d = decide(
            &DefaultPolicy::default(),
            Effect::External,
            &mail_grants(),
            &LabelSet::of([Label::Private, Label::Untrusted]),
            &Guarantees::STRONG,
        );
        assert!(matches!(d, Decision::AskHuman { .. }));
    }
}
