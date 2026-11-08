//! デーモンの設定（TOML）。

use otas_types::{Guarantees, Level};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// 設定の読み込みエラー。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// ファイルを読めない。
    #[error("設定ファイルを読めません: {0}")]
    Io(#[from] std::io::Error),
    /// 形式が不正。
    #[error("設定ファイルの形式が不正です: {0}")]
    Parse(#[from] toml::de::Error),
}

/// デーモンの設定。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 状態を置くディレクトリ（監査ログ、ソケット、タスクのログ）。所有者だけが読み書きできる権限にする。
    pub state_dir: PathBuf,
    /// サンドボックスの起動役 `otas-sandbox-exec` のパス。
    pub launcher: PathBuf,
    /// ポリシーの設定。
    #[serde(default)]
    pub policy: PolicyConfig,
    /// 動作確認用の設定。
    #[serde(default)]
    pub demo: DemoConfig,
}

/// ポリシーの設定。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// 外部に作用する操作を許すのに必要な、実行環境の保証。
    ///
    /// 既定値は Linux のサンドボックスが現在保証できる範囲（ファイルとネットワークは Strong、
    /// システムコールは Weak、スナップショットはなし）。保証を上げたら、ここも上げる。
    #[serde(default = "default_external_requires")]
    pub external_requires: Guarantees,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            external_requires: default_external_requires(),
        }
    }
}

fn default_external_requires() -> Guarantees {
    Guarantees {
        fs_isolation: Level::Strong,
        net_isolation: Level::Strong,
        syscall_filter: Level::Weak,
        snapshot: Level::Absent,
    }
}

/// 動作確認用の設定。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemoConfig {
    /// 動作確認用のツール（`demo.*`）を登録する。本番では使わない。
    #[serde(default)]
    pub tools: bool,
}

impl Config {
    /// TOML の文字列から読む。
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// ファイルから読む。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }

    /// 人間向けの窓口のソケット。
    pub fn console_socket(&self) -> PathBuf {
        self.state_dir.join("console.sock")
    }

    /// 監査ログ。
    pub fn audit_log(&self) -> PathBuf {
        self.state_dir.join("audit.jsonl")
    }

    /// タスクごとのログを置くディレクトリ。
    pub fn task_dir(&self) -> PathBuf {
        self.state_dir.join("tasks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_safe_defaults() {
        let c = Config::from_toml(
            r#"
            state_dir = "/var/lib/otas"
            launcher = "/usr/libexec/otas-sandbox-exec"
            "#,
        )
        .unwrap();
        assert!(!c.demo.tools);
        assert_eq!(c.policy.external_requires.fs_isolation, Level::Strong);
        assert_eq!(c.console_socket(), Path::new("/var/lib/otas/console.sock"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = Config::from_toml(
            r#"
            state_dir = "/x"
            launcher = "/y"
            allow_everything = true
            "#,
        );
        assert!(e.is_err());
    }

    #[test]
    fn policy_can_be_raised() {
        let c = Config::from_toml(
            r#"
            state_dir = "/x"
            launcher = "/y"
            [policy.external_requires]
            fs_isolation = "strong"
            net_isolation = "strong"
            syscall_filter = "strong"
            snapshot = "strong"
            "#,
        )
        .unwrap();
        assert_eq!(c.policy.external_requires, Guarantees::STRONG);
    }
}
