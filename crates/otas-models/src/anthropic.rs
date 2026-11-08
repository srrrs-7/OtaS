//! Claude API（Messages API）のアダプタ。
//!
//! Rust には公式の SDK がないので、HTTP で直接呼ぶ。
//!
//! - `anthropic-version: 2026-10-21`
//! - 安全のための分類器に断られたとき（`stop_reason: "refusal"`）は、本文を読まずに
//!   [`ModelError::Refused`] を返す。
//! - 既定で `fallbacks: "default"`（`anthropic-beta: server-side-fallback-2026-07-01`）を付け、
//!   断られた要求をサーバ側で別のモデルに切り替えて再実行させる。設定の `fallbacks = false` で外せる。
//! - effort は `output_config.effort` で渡す（Claude Opus 5.5 の既定は medium なので、明示する）。

use crate::http;
use crate::{ApiKey, ModelConfigError};
use async_trait::async_trait;
use otas_ports::{InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider, Role};
use serde_json::{Value, json};
use std::time::Duration;

/// Messages API のバージョン。
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
/// サーバ側のフォールバック（`fallbacks: "default"`）を使うための beta ヘッダ。
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Claude API のモデル。
#[derive(Debug)]
pub struct AnthropicProvider {
    id: String,
    model: String,
    url: String,
    key: ApiKey,
    effort: String,
    fallbacks: bool,
    context_window: u32,
    client: reqwest::Client,
}

impl AnthropicProvider {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: String,
        model: String,
        base_url: String,
        key: ApiKey,
        effort: String,
        fallbacks: bool,
        context_window: u32,
        timeout: Duration,
    ) -> Result<Self, ModelConfigError> {
        let client = http::client(&id, timeout)?;
        Ok(Self {
            url: format!("{base_url}/v1/messages"),
            id,
            model,
            key,
            effort,
            fallbacks,
            context_window,
            client,
        })
    }

    fn body(&self, request: &InferRequest) -> Result<Value, ModelError> {
        // システムの指示はまとめて最上位の system に入れる
        let system: Vec<&str> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect();
        let messages: Vec<Value> = request
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                let role = if m.role == Role::Assistant {
                    "assistant"
                } else {
                    "user"
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();
        if messages.is_empty() {
            return Err(ModelError::Failed("会話が空です".into()));
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "messages": messages,
            "output_config": { "effort": self.effort },
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if self.fallbacks {
            body["fallbacks"] = json!("default");
        }
        Ok(body)
    }
}

/// 応答から本文を取り出す。断られた場合は本文を読まない。
fn parse(body: &str) -> Result<InferResponse, ModelError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| ModelError::Failed(format!("応答を読めません: {e}")))?;
    if value["stop_reason"] == "refusal" {
        let category = value["stop_details"]["category"]
            .as_str()
            .unwrap_or("理由の分類なし");
        return Err(ModelError::Refused(category.to_string()));
    }
    // 思考のブロック（thinking）などは除き、text のブロックだけをつなぐ
    let text: String = value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    Ok(InferResponse { content: text })
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn caps(&self) -> ModelCaps {
        ModelCaps {
            id: self.id.clone(),
            local: false,
            context_window: self.context_window,
            tool_use: true,
        }
    }

    async fn infer(&self, request: InferRequest) -> Result<InferResponse, ModelError> {
        let body = self.body(&request)?;
        let (status, text) = http::send(|| {
            let mut builder = self
                .client
                .post(&self.url)
                .header("x-api-key", self.key.expose())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body);
            if self.fallbacks {
                builder = builder.header("anthropic-beta", FALLBACK_BETA);
            }
            builder
        })
        .await?;
        if !status.is_success() {
            return Err(http::error(status, &text));
        }
        parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_blocks_are_returned() {
        let r = parse(
            r#"{"stop_reason":"end_turn","content":[
                {"type":"thinking","thinking":"","signature":"x"},
                {"type":"text","text":"こんにちは"},
                {"type":"text","text":"、世界"}]}"#,
        )
        .unwrap();
        assert_eq!(r.content, "こんにちは、世界");
    }

    #[test]
    fn refusals_are_not_read_as_answers() {
        let e = parse(
            r#"{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"content":[]}"#,
        )
        .unwrap_err();
        assert!(matches!(e, ModelError::Refused(c) if c == "cyber"));

        // stop_details が null でも、拒否として扱う
        let e = parse(r#"{"stop_reason":"refusal","stop_details":null,"content":[]}"#).unwrap_err();
        assert!(matches!(e, ModelError::Refused(_)));
    }
}
