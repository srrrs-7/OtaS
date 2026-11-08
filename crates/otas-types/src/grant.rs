use crate::Action;
use serde::{Deserialize, Serialize};
use std::fmt;

/// タスクに与える権限の1単位。
///
/// 名前のパターンは完全一致か、末尾の `*` による前方一致（例：`mail.*`）だけを使える。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grant {
    /// `prefix` 以下のデータを読める。
    Read {
        /// 許可するディレクトリ（絶対パス）。
        prefix: String,
    },
    /// `prefix` 以下のデータに書ける（作業領域への書き込み）。
    Write {
        /// 許可するディレクトリ（絶対パス）。
        prefix: String,
    },
    /// パターンに合うツールを呼べる。
    Tool {
        /// ツール名のパターン。
        pattern: String,
    },
    /// パターンに合うモデルで推論できる。
    Model {
        /// モデル ID のパターン。
        pattern: String,
    },
}

impl Grant {
    /// この権限で操作が許されるか。
    pub fn permits(&self, action: &Action) -> bool {
        match (self, action) {
            (Grant::Read { prefix }, Action::ReadData { path }) => path_within(prefix, path),
            (Grant::Write { prefix }, Action::WriteData { path }) => path_within(prefix, path),
            (Grant::Tool { pattern }, Action::CallTool { tool }) => name_matches(pattern, tool),
            (Grant::Model { pattern }, Action::Infer { model }) => name_matches(pattern, model),
            _ => false,
        }
    }

    /// この権限が `other` を完全に含むか（委任のときの縮小判定に使う）。
    pub fn covers(&self, other: &Grant) -> bool {
        match (self, other) {
            (Grant::Read { prefix: p }, Grant::Read { prefix: c })
            | (Grant::Write { prefix: p }, Grant::Write { prefix: c }) => path_within(p, c),
            (Grant::Tool { pattern: p }, Grant::Tool { pattern: c })
            | (Grant::Model { pattern: p }, Grant::Model { pattern: c }) => pattern_covers(p, c),
            _ => false,
        }
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Grant::Read { prefix } => write!(f, "読み取り {prefix}"),
            Grant::Write { prefix } => write!(f, "書き込み {prefix}"),
            Grant::Tool { pattern } => write!(f, "ツール {pattern}"),
            Grant::Model { pattern } => write!(f, "モデル {pattern}"),
        }
    }
}

/// `..` や `.` を含まない絶対パスか。含むパスは常に拒否する。
fn is_clean_absolute(path: &str) -> bool {
    path.starts_with('/') && !path.split('/').any(|c| c == ".." || c == ".")
}

/// `path` が `prefix` と同じか、その配下にあるか。
/// `/data/a` は `/data/ab` を含まない。
fn path_within(prefix: &str, path: &str) -> bool {
    if !is_clean_absolute(prefix) || !is_clean_absolute(path) {
        return false;
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn pattern_covers(parent: &str, child: &str) -> bool {
    match parent.strip_suffix('*') {
        Some(prefix) => child.starts_with(prefix),
        None => parent == child,
    }
}

/// タスクが持つ権限の集合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantSet(Vec<Grant>);

impl GrantSet {
    /// 権限の集合を作る。
    pub fn new(grants: impl IntoIterator<Item = Grant>) -> Self {
        Self(grants.into_iter().collect())
    }

    /// 権限のない集合を作る。
    pub fn none() -> Self {
        Self::default()
    }

    /// どれかの権限で操作が許されるか。
    pub fn permits(&self, action: &Action) -> bool {
        self.0.iter().any(|g| g.permits(action))
    }

    /// 子タスクに渡す権限を作る。要求した権限がすべて自分の権限に含まれる場合だけ成功する。
    pub fn attenuate(&self, requested: GrantSet) -> Result<GrantSet, AttenuationError> {
        for grant in &requested.0 {
            if !self.0.iter().any(|own| own.covers(grant)) {
                return Err(AttenuationError {
                    grant: grant.clone(),
                });
            }
        }
        Ok(requested)
    }

    /// 権限を順に返す。
    pub fn iter(&self) -> impl Iterator<Item = &Grant> {
        self.0.iter()
    }
}

impl FromIterator<Grant> for GrantSet {
    fn from_iter<I: IntoIterator<Item = Grant>>(iter: I) -> Self {
        Self::new(iter)
    }
}

/// 親が持っていない権限を子に渡そうとした。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttenuationError {
    /// 渡せなかった権限。
    pub grant: Grant,
}

impl fmt::Display for AttenuationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "親タスクが持っていない権限です（{}）", self.grant)
    }
}

impl std::error::Error for AttenuationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(p: &str) -> Grant {
        Grant::Read { prefix: p.into() }
    }

    fn read_action(p: &str) -> Action {
        Action::ReadData { path: p.into() }
    }

    #[test]
    fn path_prefix_respects_component_boundary() {
        let g = read("/data/projects/a");
        assert!(g.permits(&read_action("/data/projects/a")));
        assert!(g.permits(&read_action("/data/projects/a/notes.md")));
        assert!(!g.permits(&read_action("/data/projects/ab")));
        assert!(!g.permits(&read_action("/data/projects")));
    }

    #[test]
    fn dot_dot_is_always_rejected() {
        let g = read("/data/projects/a");
        assert!(!g.permits(&read_action("/data/projects/a/../b/secret")));
        assert!(!g.permits(&read_action("/data/projects/a/./x")));
        assert!(!g.permits(&read_action("relative/path")));
    }

    #[test]
    fn tool_patterns() {
        let g = Grant::Tool {
            pattern: "mail.*".into(),
        };
        assert!(g.permits(&Action::CallTool {
            tool: "mail.read".into()
        }));
        assert!(!g.permits(&Action::CallTool {
            tool: "web.fetch".into()
        }));
        // 種類の違う操作には効かない
        assert!(!g.permits(&Action::Infer {
            model: "mail.read".into()
        }));
    }

    #[test]
    fn attenuation_allows_only_narrower_grants() {
        let parent = GrantSet::new([
            read("/data/projects"),
            Grant::Tool {
                pattern: "mail.*".into(),
            },
        ]);

        let ok = parent.attenuate(GrantSet::new([
            read("/data/projects/a"),
            Grant::Tool {
                pattern: "mail.read".into(),
            },
        ]));
        assert!(ok.is_ok());

        let wider_path = parent.attenuate(GrantSet::new([read("/data")]));
        assert!(wider_path.is_err());

        let wider_tool = parent.attenuate(GrantSet::new([Grant::Tool {
            pattern: "mail*".into(),
        }]));
        assert!(wider_tool.is_err());

        let other_kind = parent.attenuate(GrantSet::new([Grant::Write {
            prefix: "/data/projects/a".into(),
        }]));
        assert!(other_kind.is_err());
    }
}
