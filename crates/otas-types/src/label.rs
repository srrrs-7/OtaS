use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// データの出所や性質を表すラベル。
///
/// タスクは読んだデータのラベルを積み上げ、書いたものや送るものにそのラベルを引き継ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    /// 信頼できない入力。Web ページ、外部から来たイベント、他人が書いた文書など。
    Untrusted,
    /// 私的データ。メール、個人のファイル、予定など。
    Private,
    /// 秘密情報。認証情報や鍵など。外部に作用するタスクへは決して流さない。
    Secret,
}

impl Label {
    /// 機密性のあるデータか（私的データまたは秘密情報）。
    pub fn is_sensitive(self) -> bool {
        matches!(self, Label::Private | Label::Secret)
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Label::Untrusted => "信頼できない",
            Label::Private => "私的",
            Label::Secret => "秘密",
        };
        f.write_str(s)
    }
}

/// ラベルの集合。結合（和集合）だけができ、ラベルを取り除く操作は公開しない。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LabelSet(BTreeSet<Label>);

impl LabelSet {
    /// 空の集合を作る。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 指定したラベルからなる集合を作る。
    pub fn of(labels: impl IntoIterator<Item = Label>) -> Self {
        Self(labels.into_iter().collect())
    }

    /// ラベルを含むか。
    pub fn contains(&self, label: Label) -> bool {
        self.0.contains(&label)
    }

    /// 他の集合のラベルをすべて取り込む。
    pub fn join(&mut self, other: &LabelSet) {
        self.0.extend(other.0.iter().copied());
    }

    /// 他の集合と結合した新しい集合を返す。
    pub fn joined(&self, other: &LabelSet) -> LabelSet {
        let mut out = self.clone();
        out.join(other);
        out
    }

    /// 信頼できない入力に触れているか。
    pub fn has_untrusted(&self) -> bool {
        self.contains(Label::Untrusted)
    }

    /// 機密性のあるデータに触れているか。
    pub fn has_sensitive(&self) -> bool {
        self.0.iter().any(|l| l.is_sensitive())
    }

    /// ラベルが1つもないか。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// ラベルを順に返す。
    pub fn iter(&self) -> impl Iterator<Item = Label> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<Label> for LabelSet {
    fn from_iter<I: IntoIterator<Item = Label>>(iter: I) -> Self {
        Self::of(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_is_union() {
        let mut a = LabelSet::of([Label::Private]);
        a.join(&LabelSet::of([Label::Untrusted]));
        assert!(a.contains(Label::Private));
        assert!(a.contains(Label::Untrusted));
        assert!(a.has_sensitive());
        assert!(a.has_untrusted());
    }

    #[test]
    fn serializes_as_plain_list() {
        let s = LabelSet::of([Label::Untrusted, Label::Private]);
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"["untrusted","private"]"#
        );
    }
}
