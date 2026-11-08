//! OpenAI 互換の Chat Completions API のアダプタ。
//!
//! Ollama（`http://127.0.0.1:11434/v1`）、llama.cpp の `llama-server`、vLLM など、
//! `POST {base_url}/chat/completions` を受け付けるサーバに使う。ツールの呼び出し（function calling）に対応する。

use crate::http;
use crate::{ApiKey, ModelConfigError};
use async_trait::async_trait;
use otas_ports::{
    InferRequest, InferResponse, ModelCaps, ModelError, ModelProvider, Role, StopReason, ToolCall,
};
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
        let mut messages: Vec<Value> = Vec::new();
        for m in &request.messages {
            match m.role {
                Role::System => messages.push(json!({ "role": "system", "content": m.content })),
                Role::Assistant => {
                    let mut msg = json!({ "role": "assistant", "content": m.content });
                    if !m.tool_calls.is_empty() {
                        msg["tool_calls"] = m
                            .tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": { "name": c.name, "arguments": c.input.to_string() },
                                })
                            })
                            .collect();
                    }
                    messages.push(msg);
                }
                Role::User => {
                    for r in &m.tool_results {
                        messages.push(json!({
                            "role": "tool", "tool_call_id": r.call_id, "content": r.content
                        }));
                    }
                    if !m.content.is_empty() || m.tool_results.is_empty() {
                        messages.push(json!({ "role": "user", "content": m.content }));
                    }
                }
            }
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
        });
        if !request.tools.is_empty() {
            body["tools"] = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": { "name": t.name, "description": t.description, "parameters": t.input_schema },
                    })
                })
                .collect();
        }
        Ok(body)
    }
}

fn parse(body: &str) -> Result<InferResponse, ModelError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| ModelError::Failed(format!("応答を読めません: {e}")))?;
    let choice = &value["choices"][0];
    let finish = choice["finish_reason"].as_str().unwrap_or("");
    if finish == "content_filter" {
        return Err(ModelError::Refused("content_filter".into()));
    }
    let message = &choice["message"];
    if message.is_null() {
        return Err(ModelError::Failed("応答に本文がありません".into()));
    }
    let tool_calls: Vec<ToolCall> = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let arguments = c["function"]["arguments"].as_str().unwrap_or("{}");
            ToolCall {
                id: c["id"].as_str().unwrap_or("").to_string(),
                name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                input: serde_json::from_str(arguments).unwrap_or_else(|_| json!({})),
            }
        })
        .collect();
    Ok(InferResponse {
        content: message["content"].as_str().unwrap_or("").to_string(),
        stop_reason: match finish {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "stop" => StopReason::EndTurn,
            _ if !tool_calls.is_empty() => StopReason::ToolUse,
            _ => StopReason::Other,
        },
        tool_calls,
        provider_raw: None,
    })
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn caps(&self) -> ModelCaps {
        ModelCaps {
            id: self.id.clone(),
            local: self.local,
            context_window: self.context_window,
            tool_use: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_are_read() {
        let r = parse(
            r#"{"choices":[{"finish_reason":"tool_calls","message":{"content":null,
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"fs__read","arguments":"{\"path\":\"a.md\"}"}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls[0].name, "fs__read");
        assert_eq!(r.tool_calls[0].input, json!({"path": "a.md"}));
        assert_eq!(r.content, "");
    }
}
