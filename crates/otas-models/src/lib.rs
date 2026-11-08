//! OtaS のモデルのアダプタ。
//!
//! どれも [`ModelProvider`] として同じ形で扱え、ブローカーの判定（ラベルと Rule of Two）を通ってから呼ばれる。
//!
//! | プロバイダ | 用途 |
//! |---|---|
//! | `anthropic` | Claude API（Messages API）。常に外部への作用として扱う |
//! | `openai_compatible` | OpenAI 互換の Chat Completions API。Ollama、llama.cpp の `llama-server`、vLLM などのローカルモデルにも使う |
//!
//! # 安全性の要点
//!
//! - API キーはデーモンの中だけに置き、サンドボックスのエージェントには渡さない。
//!   設定ファイルにはキーそのものを書かず、環境変数名かファイルのパスを書く。
//! - `local = true`（端末の中で推論する）と申告できるのは、接続先がループバックアドレスの場合だけ。
//!   リモートのサーバを「ローカル」と偽ると、私的データがラベルの判定なしに外に出てしまうため。
//! - 暗号化しない `http` はループバックの場合だけ許す。
//! - リダイレクトはたどらない（API キーのヘッダを別のホストに送らないため）。
//! - エラーメッセージに API キーを含めない。

mod anthropic;
mod http;
mod openai;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiCompatibleProvider;

use otas_ports::ModelProvider;
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Claude API で使う既定のモデル。
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";

/// プロバイダの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Claude API（Messages API）。
    Anthropic,
    /// OpenAI 互換の Chat Completions API。
    OpenaiCompatible,
}

/// 1つのモデルの設定（デーモンの設定ファイルの `[[models]]`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// OtaS の中での ID。エージェントはこの ID でモデルを指定し、権限もこの ID で与える。
    pub id: String,
    /// プロバイダ。
    pub provider: Provider,
    /// API に渡すモデル名。`anthropic` で省略すると `claude-opus-5-5`。
    #[serde(default)]
    pub model: Option<String>,
    /// API の URL。`anthropic` で省略すると `https://api.anthropic.com`。
    /// `openai_compatible` では必須（例：`http://127.0.0.1:11434/v1`）。
    #[serde(default)]
    pub base_url: Option<String>,
    /// API キーを読む環境変数の名前。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// API キーを読むファイルのパス（systemd の credentials など）。
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    /// 端末の中で推論するか。ループバックアドレスの `openai_compatible` だけが `true` にできる。
    #[serde(default)]
    pub local: bool,
    /// コンテキスト長（トークン数）。ポリシーやエージェントの参考情報。
    #[serde(default)]
    pub context_window: Option<u32>,
    /// `anthropic` の effort（`low`・`medium`・`high`・`xhigh`・`max`）。省略すると `high`。
    #[serde(default)]
    pub effort: Option<String>,
    /// `anthropic` で、安全のための分類器に断られたときにサーバ側で別のモデルに切り替えるか。
    /// 省略すると有効（`fallbacks: "default"`）。
    #[serde(default)]
    pub fallbacks: Option<bool>,
    /// 1回の要求のタイムアウト（秒）。省略すると 600 秒。
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// 設定のエラー。
#[derive(Debug, thiserror::Error)]
pub enum ModelConfigError {
    /// 設定が不正。
    #[error("モデル {id} の設定が不正です: {reason}")]
    Invalid {
        /// モデルの ID。
        id: String,
        /// 理由。
        reason: String,
    },
    /// API キーが見つからない。
    #[error("モデル {id} の API キーが見つかりません: {reason}")]
    MissingKey {
        /// モデルの ID。
        id: String,
        /// 理由。
        reason: String,
    },
}

/// API キー。`Debug` や `Display` で中身を出さない。
#[derive(Clone)]
pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

/// 接続先の URL を確かめた結果。
pub(crate) struct Endpoint {
    pub(crate) base: String,
    pub(crate) loopback: bool,
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn endpoint(id: &str, url: &str) -> Result<Endpoint, ModelConfigError> {
    let invalid = |reason: String| ModelConfigError::Invalid {
        id: id.to_string(),
        reason,
    };
    let parsed =
        reqwest::Url::parse(url).map_err(|e| invalid(format!("base_url を読めません: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| invalid("base_url にホストがありません".into()))?;
    let loopback = is_loopback(host);
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(invalid(
                "暗号化しない http はループバックアドレスにだけ使えます".into(),
            ));
        }
        other => return Err(invalid(format!("使えないスキームです: {other}"))),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "base_url に認証情報を書かないでください（api_key_env か api_key_file を使う）".into(),
        ));
    }
    Ok(Endpoint {
        base: url.trim_end_matches('/').to_string(),
        loopback,
    })
}

fn api_key(
    config: &ModelConfig,
    secrets: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ApiKey>, ModelConfigError> {
    let missing = |reason: String| ModelConfigError::MissingKey {
        id: config.id.clone(),
        reason,
    };
    let key = match (&config.api_key_env, &config.api_key_file) {
        (Some(_), Some(_)) => {
            return Err(ModelConfigError::Invalid {
                id: config.id.clone(),
                reason: "api_key_env と api_key_file はどちらか一方だけ指定してください".into(),
            });
        }
        (Some(name), None) => Some(
            secrets(name)
                .ok_or_else(|| missing(format!("環境変数 {name} が設定されていません")))?,
        ),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| missing(format!("{} を読めません: {e}", path.display())))?,
        ),
        (None, None) => None,
    };
    match key.map(|k| k.trim().to_string()) {
        Some(k) if k.is_empty() => Err(missing("API キーが空です".into())),
        Some(k) => Ok(Some(ApiKey(k))),
        None => Ok(None),
    }
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// 設定からモデルを作る。`secrets` は環境変数の名前から値を引く関数（テストでは差し替える）。
pub fn build(
    config: &ModelConfig,
    secrets: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ModelProvider>, ModelConfigError> {
    let invalid = |reason: &str| ModelConfigError::Invalid {
        id: config.id.clone(),
        reason: reason.to_string(),
    };
    if config.id.trim().is_empty() {
        return Err(invalid("id が空です"));
    }
    let timeout = Duration::from_secs(config.timeout_seconds.unwrap_or(600));
    let key = api_key(config, secrets)?;

    match config.provider {
        Provider::Anthropic => {
            if config.local {
                return Err(invalid(
                    "Claude API は端末の外で推論するので local = true にはできません",
                ));
            }
            let key = key.ok_or_else(|| ModelConfigError::MissingKey {
                id: config.id.clone(),
                reason: "api_key_env か api_key_file を指定してください".into(),
            })?;
            let effort = config.effort.clone().unwrap_or_else(|| "high".into());
            if !EFFORTS.contains(&effort.as_str()) {
                return Err(invalid(
                    "effort は low・medium・high・xhigh・max のどれかです",
                ));
            }
            let endpoint = endpoint(
                &config.id,
                config
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.anthropic.com"),
            )?;
            Ok(Arc::new(AnthropicProvider::new(
                config.id.clone(),
                config
                    .model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into()),
                endpoint.base,
                key,
                effort,
                config.fallbacks.unwrap_or(true),
                config.context_window.unwrap_or(1_000_000),
                timeout,
            )?))
        }
        Provider::OpenaiCompatible => {
            if config.effort.is_some() || config.fallbacks.is_some() {
                return Err(invalid("effort と fallbacks は anthropic でだけ使えます"));
            }
            let model = config
                .model
                .clone()
                .ok_or_else(|| invalid("openai_compatible では model が必須です"))?;
            let base_url = config
                .base_url
                .as_deref()
                .ok_or_else(|| invalid("openai_compatible では base_url が必須です"))?;
            let endpoint = endpoint(&config.id, base_url)?;
            if config.local && !endpoint.loopback {
                return Err(invalid(
                    "local = true にできるのは、接続先がループバックアドレスの場合だけです",
                ));
            }
            Ok(Arc::new(OpenAiCompatibleProvider::new(
                config.id.clone(),
                model,
                endpoint.base,
                key,
                config.local,
                config.context_window.unwrap_or(8192),
                timeout,
            )?))
        }
    }
}

/// 設定からモデルを作る。API キーはデーモンの環境変数から読む。
pub fn build_from_env(config: &ModelConfig) -> Result<Arc<dyn ModelProvider>, ModelConfigError> {
    build(config, &|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> ModelConfig {
        toml::from_str(text).unwrap()
    }

    fn no_secrets(_: &str) -> Option<String> {
        None
    }

    fn key(_: &str) -> Option<String> {
        Some("sk-test".into())
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("[::1]"));
        assert!(is_loopback("localhost"));
        assert!(!is_loopback("192.168.1.10"));
        assert!(!is_loopback("api.example.com"));
    }

    #[test]
    fn a_remote_server_cannot_claim_to_be_local() {
        let c = cfg(r#"
            id = "fake-local"
            provider = "openai_compatible"
            model = "m"
            base_url = "https://llm.example.com/v1"
            local = true
        "#);
        assert!(matches!(
            build(&c, &no_secrets),
            Err(ModelConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn plain_http_is_only_for_loopback() {
        let remote = cfg(r#"
            id = "r"
            provider = "openai_compatible"
            model = "m"
            base_url = "http://llm.example.com/v1"
        "#);
        assert!(build(&remote, &no_secrets).is_err());

        let local = cfg(r#"
            id = "l"
            provider = "openai_compatible"
            model = "llama"
            base_url = "http://127.0.0.1:11434/v1"
            local = true
        "#);
        let provider = build(&local, &no_secrets).unwrap();
        assert!(provider.caps().local);
    }

    #[test]
    fn claude_needs_a_key_and_is_never_local() {
        let c = cfg(r#"
            id = "claude"
            provider = "anthropic"
            api_key_env = "ANTHROPIC_API_KEY"
        "#);
        assert!(matches!(
            build(&c, &no_secrets),
            Err(ModelConfigError::MissingKey { .. })
        ));
        let provider = build(&c, &key).unwrap();
        assert!(!provider.caps().local);
        assert_eq!(provider.caps().id, "claude");

        let local = cfg(r#"
            id = "claude"
            provider = "anthropic"
            api_key_env = "K"
            local = true
        "#);
        assert!(build(&local, &key).is_err());
    }

    #[test]
    fn credentials_are_not_allowed_in_the_url() {
        let c = cfg(r#"
            id = "x"
            provider = "openai_compatible"
            model = "m"
            base_url = "https://user:secret@llm.example.com/v1"
        "#);
        assert!(build(&c, &no_secrets).is_err());
    }

    #[test]
    fn invalid_effort_is_rejected() {
        let c = cfg(r#"
            id = "claude"
            provider = "anthropic"
            api_key_env = "K"
            effort = "extreme"
        "#);
        assert!(build(&c, &key).is_err());
    }

    #[test]
    fn keys_are_never_printed() {
        let k = ApiKey("sk-very-secret".into());
        assert!(!format!("{k:?}").contains("secret"));
    }
}
