//! OpenAI 互換の Chat Completions API のアダプタ。
//!
//! Ollama（`http://127.0.0.1:11434/v1`）、llama.cpp の `llama-server`、vLLM など、
//! `POST {base_url}/chat/completions` を受け付けるサーバに使う。

use crate::http;
use crate::{ApiKey, ModelConfigError};
use async_trait::async_trait;
use otas_ports::{InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider, Role};
use serde_json::{Value, json};
use std::time::Duration;

/// OpenAI 互換 API のモデル。
#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    id: String,
    model: String,
    url: String,
    key: Option<ApiKey>,
    local: bool,
    context_window: u32,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn new(
        id: String,
        model: String,
        base_url: String,
        key: Option<ApiKey>,
        local: bool,
        context_window: u32,
        timeout: Duration,
    ) -> Result<Self, ModelConfigError> {
        let client = http::client(&id, timeout)?;
        Ok(Self {
            url: format!("{base_url}/chat/completions"),
            id,
            model,
            key,
            local,
            context_window,
            client,
        })
    }

    fn body(&self, request: &InferRequest) -> Result<Value, ModelError> {
        if request.messages.is_empty() {
            return Err(ModelError::Failed("会話が空です".into()));
        }
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();
        Ok(json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
        }))
    }
}

fn parse(body: &str) -> Result<InferResponse, ModelError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| ModelError::Failed(format!("応答を読めません: {e}")))?;
    let choice = &value["choices"][0];
    if choice["finish_reason"] == "content_filter" {
        return Err(ModelError::Refused("content_filter".into()));
    }
    let content = choice["message"]["content"]
        .as_str()
        .ok_or_else(|| ModelError::Failed("応答に本文がありません".into()))?;
    Ok(InferResponse {
        content: content.to_string(),
    })
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn caps(&self) -> ModelCaps {
        ModelCaps {
            id: self.id.clone(),
            local: self.local,
            context_window: self.context_window,
            tool_use: false,
        }
    }

    async fn infer(&self, request: InferRequest) -> Result<InferResponse, ModelError> {
        let body = self.body(&request)?;
        let (status, text) = http::send(|| {
            let mut builder = self.client.post(&self.url).json(&body);
            if let Some(key) = &self.key {
                builder = builder.bearer_auth(key.expose());
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
