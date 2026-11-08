//! デーモンの設定（TOML）。

use otas_types::{Guarantees, LabelSet, Level};
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
    /// サンドボックスの設定。
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// データ（正本）の設定。
    #[serde(default)]
    pub data: DataConfig,
    /// モデル。API キーはデーモンの中だけに置き、エージェントには渡さない。
    #[serde(default)]
    pub models: Vec<otas_models::ModelConfig>,
    /// 動作確認用の設定。
    #[serde(default)]
    pub demo: DemoConfig,
}

/// サンドボックスの方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Landlock と seccomp だけ（特権不要）。作業領域はコピー方式で、`OTAS_DATA` の場所に見える。
    #[default]
    Landlock,
    /// 名前空間も使う（デーモンを root で動かす）。作業領域は OverlayFS で、`/data` として見える。
    Namespace,
    /// 名前空間をユーザー名前空間の中で使う（デーモンを一般ユーザーで動かす）。
    /// タスクはデーモンと同じユーザーで動くが、ケーパビリティは持たない。作業領域は `namespace` と同じ。
    Rootless,
}

/// サンドボックスの設定。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// 方式。
    #[serde(default)]
    pub mode: SandboxMode,
    /// `namespace` のとき、タスクを動かすユーザー（0 以外。データの所有者にもなる）。
    #[serde(default)]
    pub uid: Option<u32>,
    /// `namespace` のとき、タスクを動かすグループ（0 以外）。
    #[serde(default)]
    pub gid: Option<u32>,
    /// デーモンに任された cgroup（systemd の `Delegate=yes` のユニットの cgroup など）。
    /// 指定すると、タスクごとに cgroup を作り、メモリ・プロセス数・CPU の上限をかけられる。
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
}

/// データ（正本）の設定。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    /// 版の方式（`hardlink` か `btrfs`）。`btrfs` は `state_dir` を Btrfs 上に置き、
    /// サンドボックスを `namespace` にする必要がある。
    #[serde(default)]
    pub versions: otas_workspace::VersionBackend,
    /// 範囲ごとのラベル。その範囲を作業領域に見せたタスクは、このラベルを持つ。
    #[serde(default)]
    pub labels: Vec<LabelRule>,
}

/// 範囲に付けるラベル。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelRule {
    /// 正本上の論理パス（`/data/...`）。
    pub path: String,
    /// 付けるラベル（`private`、`untrusted`、`secret`）。
    pub labels: LabelSet,
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

    /// 正本・版・作業領域を置くディレクトリ。
    pub fn store_dir(&self) -> PathBuf {
        self.state_dir.join("store")
    }

    /// 名前空間のサンドボックスが、タスクごとの新しいルートを組み立てるディレクトリ。
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// `namespace` のときのタスクのユーザーとグループ。
    pub fn task_owner(&self) -> Option<(u32, u32)> {
        match self.sandbox.mode {
            SandboxMode::Landlock | SandboxMode::Rootless => None,
            SandboxMode::Namespace => Some((self.sandbox.uid?, self.sandbox.gid?)),
        }
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
    fn data_labels_are_read() {
        let c = Config::from_toml(
            r#"
            state_dir = "/x"
            launcher = "/y"
            [[data.labels]]
            path = "/data/mail"
            labels = ["private"]
            "#,
        )
        .unwrap();
        assert_eq!(c.data.labels[0].path, "/data/mail");
        assert!(c.data.labels[0].labels.contains(otas_types::Label::Private));
    }

    #[test]
    fn models_are_read() {
        let c = Config::from_toml(
            r#"
            state_dir = "/x"
            launcher = "/y"
            [[models]]
            id = "claude"
            provider = "anthropic"
            api_key_env = "ANTHROPIC_API_KEY"
            [[models]]
            id = "local"
            provider = "openai_compatible"
            model = "llama3"
            base_url = "http://127.0.0.1:11434/v1"
            local = true
            "#,
        )
        .unwrap();
        assert_eq!(c.models.len(), 2);
        assert_eq!(c.models[1].id, "local");
    }

    #[test]
    fn sandbox_and_versions_are_read() {
        let c = Config::from_toml(
            r#"
            state_dir = "/x"
            launcher = "/y"
            [sandbox]
            mode = "namespace"
            uid = 1000
            gid = 1000
            [data]
            versions = "btrfs"
            "#,
        )
        .unwrap();
        assert_eq!(c.sandbox.mode, SandboxMode::Namespace);
        assert_eq!(c.task_owner(), Some((1000, 1000)));
        assert_eq!(c.data.versions, otas_workspace::VersionBackend::Btrfs);
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
