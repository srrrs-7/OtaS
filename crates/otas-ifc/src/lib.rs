//! OtaS の情報フロー制御。
//!
//! タスクは読んだデータのラベルを積み上げる。そのうえで、Meta の「Agents Rule of Two」に従い、
//! 1つのタスクが次の3つを同時に満たす操作は、人間の承認なしには実行させない。
//!
//! 1. 信頼できない入力に触れている
//! 2. 機密性のあるデータに触れている
//! 3. 外部に作用する
//!
//! さらに、秘密情報に触れたタスクは、承認があっても外部に作用させない。

use otas_types::{Effect, Label, LabelSet};

/// Rule of Two の3つの条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskProfile {
    /// 信頼できない入力に触れている。
    pub untrusted_input: bool,
    /// 機密性のあるデータに触れている。
    pub sensitive_access: bool,
    /// 外部に作用する。
    pub external_effect: bool,
}

impl RiskProfile {
    /// タスクのラベルと操作の影響範囲から評価する。
    pub fn assess(labels: &LabelSet, effect: Effect) -> Self {
        Self {
            untrusted_input: labels.has_untrusted(),
            sensitive_access: labels.has_sensitive(),
            external_effect: effect == Effect::External,
        }
    }

    /// 満たしている条件の数。
    pub fn count(&self) -> usize {
        [
            self.untrusted_input,
            self.sensitive_access,
            self.external_effect,
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }
}

/// 情報フロー制御の判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 問題ない。
    Ok,
    /// 人間の承認が必要。
    RequiresApproval {
        /// 人間が読める理由。
        reason: String,
    },
    /// 承認があっても実行させない。
    Forbidden {
        /// 人間が読める理由。
        reason: String,
    },
}

/// タスクのラベルと操作の影響範囲から判定する。
pub fn evaluate(labels: &LabelSet, effect: Effect) -> Verdict {
    if effect == Effect::External && labels.contains(Label::Secret) {
        return Verdict::Forbidden {
            reason: "秘密情報に触れたタスクは外部に作用できません".into(),
        };
    }
    if RiskProfile::assess(labels, effect).count() == 3 {
        return Verdict::RequiresApproval {
            reason: "信頼できない入力・機密性のあるデータ・外部への作用の3つがそろっています（Rule of Two）"
                .into(),
        };
    }
    Verdict::Ok
}

/// タスクが観測したデータのラベルを、タスクのラベルに取り込む。
/// ラベルは増えるだけで、減ることはない。
pub fn propagate(task_labels: &mut LabelSet, observed: &LabelSet) {
    task_labels.join(observed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_conditions_are_fine() {
        let labels = LabelSet::of([Label::Private]);
        assert_eq!(evaluate(&labels, Effect::External), Verdict::Ok);

        let labels = LabelSet::of([Label::Private, Label::Untrusted]);
        assert_eq!(evaluate(&labels, Effect::Internal), Verdict::Ok);
    }

    #[test]
    fn three_conditions_require_approval() {
        let labels = LabelSet::of([Label::Private, Label::Untrusted]);
        assert!(matches!(
            evaluate(&labels, Effect::External),
            Verdict::RequiresApproval { .. }
        ));
    }

    #[test]
    fn secrets_never_leave() {
        let labels = LabelSet::of([Label::Secret]);
        assert!(matches!(
            evaluate(&labels, Effect::External),
            Verdict::Forbidden { .. }
        ));
        assert_eq!(evaluate(&labels, Effect::Internal), Verdict::Ok);
    }

    #[test]
    fn propagation_only_grows() {
        let mut task = LabelSet::of([Label::Private]);
        propagate(&mut task, &LabelSet::empty());
        assert!(task.contains(Label::Private));
        propagate(&mut task, &LabelSet::of([Label::Untrusted]));
        assert!(task.contains(Label::Private) && task.contains(Label::Untrusted));
    }
}
