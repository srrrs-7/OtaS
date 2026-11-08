//! Claude API（Messages API）のアダプタ。
//!
//! Rust には公式の SDK がないので、HTTP で直接呼ぶ。
//!
//! - `anthropic-version: 2026-10-21`
//! - ストリーミング（`stream: true`）で受け取り、最後まで組み立ててから返す。
//!   長い応答でも、接続が途中で切られないようにするため。
//! - 安全のための分類器に断られたとき（`stop_reason: "refusal"`）は、本文を読まずに
//!   [`ModelError::Refused`] を返す。
//! - 既定で `fallbacks: "default"`（`anthropic-beta: server-side-fallback-2026-07-01`）を付け、
//!   断られた要求をサーバ側で別のモデルに切り替えて再実行させる。設定の `fallbacks = false` で外せる。
//! - effort は `output_config.effort` で渡す（Claude Opus 5.5 の既定は medium なので、明示する）。
//! - ツールの呼び出し（tool use）に対応する。応答の思考のブロック（thinking）は、
//!   次の要求の assistant に変えずに送り返す（`provider_raw`）。

use crate::http;
use crate::{ApiKey, ModelConfigError};
use async_trait::async_trait;
use otas_ports::{
    InferRequest, InferResponse, Message, ModelCaps, ModelError, ModelProvider, Role, StopReason,
    ToolCall,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// Messages API のバージョン。
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
/// サーバ側のフォールバック（`fallbacks: "default"`）を使うための beta ヘッダ。
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// 次の要求で送り返すブロックの種類（それ以外は送り返さない）。
const REPLAYED_BLOCKS: [&str; 4] = ["text", "thinking", "redacted_thinking", "tool_use"];

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

    fn message(m: &Message) -> Value {
        match m.role {
            Role::Assistant => {
                // 前の応答をそのまま送り返す（思考のブロックを変えないため）
                if let Some(raw) = &m.provider_raw {
                    return json!({ "role": "assistant", "content": raw });
                }
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for call in &m.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use", "id": call.id, "name": call.name, "input": call.input
                    }));
                }
                if blocks.is_empty() {
                    json!({ "role": "assistant", "content": m.content })
                } else {
                    json!({ "role": "assistant", "content": blocks })
                }
            }
            _ => {
                if m.tool_results.is_empty() {
                    return json!({ "role": "user", "content": m.content });
                }
                let mut blocks: Vec<Value> = m
                    .tool_results
                    .iter()
                    .map(|r| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": r.call_id,
                            "content": r.content,
                            "is_error": r.is_error,
                        })
                    })
                    .collect();
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                json!({ "role": "user", "content": blocks })
            }
        }
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
            .map(Self::message)
            .collect();
        if messages.is_empty() {
            return Err(ModelError::Failed("会話が空です".into()));
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "messages": messages,
            "output_config": { "effort": self.effort },
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if !request.tools.is_empty() {
            body["tools"] = request
                .tools
                .iter()
                .map(|t| {
                    json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })
                })
                .collect();
        }
        if self.fallbacks {
            body["fallbacks"] = json!("default");
        }
        Ok(body)
    }
}

/// 組み立て終わったメッセージ（`content` の配列と、終わった理由）から応答を作る。
fn finish(
    blocks: Vec<Value>,
    stop_reason: &str,
    stop_details: &Value,
) -> Result<InferResponse, ModelError> {
    if stop_reason == "refusal" {
        let category = stop_details["category"]
            .as_str()
            .unwrap_or("理由の分類なし");
        return Err(ModelError::Refused(category.to_string()));
    }
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or("").to_string(),
                name: block["name"].as_str().unwrap_or("").to_string(),
                input: block["input"].clone(),
            }),
            _ => {}
        }
    }
    let raw: Vec<Value> = blocks
        .into_iter()
        .filter(|b| REPLAYED_BLOCKS.contains(&b["type"].as_str().unwrap_or("")))
        .collect();
    Ok(InferResponse {
        content: text,
        tool_calls,
        stop_reason: match stop_reason {
            "end_turn" | "stop_sequence" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            _ => StopReason::Other,
        },
        provider_raw: Some(Value::Array(raw)),
    })
}

/// ストリーミングしない形（1つの JSON）の応答を読む。
fn parse_message(body: &str) -> Result<InferResponse, ModelError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| ModelError::Failed(format!("応答を読めません: {e}")))?;
    let blocks = value["content"].as_array().cloned().unwrap_or_default();
    finish(
        blocks,
        value["stop_reason"].as_str().unwrap_or(""),
        &value["stop_details"],
    )
}

/// ストリーミング（Server-Sent Events）の応答を組み立てる。
fn parse_stream(body: &str) -> Result<InferResponse, ModelError> {
    let mut blocks: BTreeMap<u64, Value> = BTreeMap::new();
    let mut partial_json: BTreeMap<u64, String> = BTreeMap::new();
    let mut stop_reason = String::new();
    let mut stop_details = Value::Null;

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let event: Value = serde_json::from_str(data.trim())
            .map_err(|e| ModelError::Failed(format!("ストリームを読めません: {e}")))?;
        let index = event["index"].as_u64().unwrap_or(0);
        match event["type"].as_str() {
            Some("content_block_start") => {
                blocks.insert(index, event["content_block"].clone());
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                let Some(block) = blocks.get_mut(&index) else {
                    continue;
                };
                match delta["type"].as_str() {
                    Some("text_delta") => append(block, "text", &delta["text"]),
                    Some("thinking_delta") => append(block, "thinking", &delta["thinking"]),
                    Some("signature_delta") => append(block, "signature", &delta["signature"]),
                    Some("input_json_delta") => partial_json
                        .entry(index)
                        .or_default()
                        .push_str(delta["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(block) = blocks.get_mut(&index) {
                    if block["type"] == "tool_use" {
                        let json = partial_json.remove(&index).unwrap_or_default();
                        block["input"] = if json.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&json).map_err(|e| {
                                ModelError::Failed(format!("ツールの引数を読めません: {e}"))
                            })?
                        };
                    }
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    stop_reason = reason.to_string();
                }
                if !event["delta"]["stop_details"].is_null() {
                    stop_details = event["delta"]["stop_details"].clone();
                }
            }
            Some("error") => {
                let kind = event["error"]["type"].as_str().unwrap_or("error");
                let message = event["error"]["message"].as_str().unwrap_or("");
                let text = format!("ストリームの途中でエラー {kind}: {message}");
                return Err(if kind == "overloaded_error" || kind == "api_error" {
                    ModelError::Unavailable(text)
                } else {
                    ModelError::Failed(text)
                });
            }
            _ => {}
        }
    }
    finish(blocks.into_values().collect(), &stop_reason, &stop_details)
}

fn append(block: &mut Value, field: &str, piece: &Value) {
    let current = block[field].as_str().unwrap_or("").to_string();
    block[field] = json!(current + piece.as_str().unwrap_or(""));
}

/// 応答を読む。ストリーミングの形でも、1つの JSON の形でも受け付ける。
fn parse(body: &str) -> Result<InferResponse, ModelError> {
    if body.trim_start().starts_with('{') {
        parse_message(body)
    } else {
        parse_stream(body)
    }
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

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
            .collect()
    }

    #[test]
    fn only_text_blocks_become_the_answer() {
        let r = parse(
            r#"{"stop_reason":"end_turn","content":[
                {"type":"thinking","thinking":"","signature":"x"},
                {"type":"text","text":"こんにちは"},
                {"type":"text","text":"、世界"}]}"#,
        )
        .unwrap();
        assert_eq!(r.content, "こんにちは、世界");
        // 思考のブロックは送り返すために残す
        assert_eq!(r.provider_raw.unwrap()[0]["type"], "thinking");
    }

    #[test]
    fn refusals_are_not_read_as_answers() {
        let e = parse(
            r#"{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"},"content":[]}"#,
        )
        .unwrap_err();
        assert!(matches!(e, ModelError::Refused(c) if c == "cyber"));

        let e = parse(r#"{"stop_reason":"refusal","stop_details":null,"content":[]}"#).unwrap_err();
        assert!(matches!(e, ModelError::Refused(_)));
    }

    #[test]
    fn streams_are_assembled_with_thinking_and_tool_use() {
        let body = sse(&[
            json!({"type": "message_start", "message": {"id": "msg_1"}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "sig-1"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "text_delta", "text": "読みます"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2,
                   "content_block": {"type": "tool_use", "id": "toolu_1", "name": "fs__read", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"no"}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "tes.md\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            json!({"type": "message_stop"}),
        ]);
        let r = parse(&body).unwrap();
        assert_eq!(r.content, "読みます");
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "toolu_1");
        assert_eq!(r.tool_calls[0].input, json!({"path": "notes.md"}));
        let raw = r.provider_raw.unwrap();
        assert_eq!(raw[0]["signature"], "sig-1");
        assert_eq!(raw[2]["input"], json!({"path": "notes.md"}));
    }

    #[test]
    fn stream_refusals_and_errors_are_reported() {
        let refused = sse(&[
            json!({"type": "message_start", "message": {}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal",
                   "stop_details": {"category": "bio"}}}),
        ]);
        assert!(matches!(parse(&refused), Err(ModelError::Refused(c)) if c == "bio"));

        let overloaded = sse(&[json!({"type": "error",
            "error": {"type": "overloaded_error", "message": "busy"}})]);
        assert!(matches!(
            parse(&overloaded),
            Err(ModelError::Unavailable(_))
        ));
    }

    #[test]
    fn tool_results_and_replayed_assistant_turns_are_encoded() {
        let assistant = Message {
            role: Role::Assistant,
            provider_raw: Some(
                json!([{"type": "thinking", "thinking": "", "signature": "s"},
                                      {"type": "tool_use", "id": "t1", "name": "x", "input": {}}]),
            ),
            ..Message::default()
        };
        let encoded = AnthropicProvider::message(&assistant);
        assert_eq!(encoded["content"][0]["signature"], "s");

        let user = Message {
            role: Role::User,
            tool_results: vec![otas_ports::ToolResult {
                call_id: "t1".into(),
                content: "結果".into(),
                is_error: false,
            }],
            ..Message::default()
        };
        let encoded = AnthropicProvider::message(&user);
        assert_eq!(encoded["content"][0]["type"], "tool_result");
        assert_eq!(encoded["content"][0]["tool_use_id"], "t1");
    }
}
