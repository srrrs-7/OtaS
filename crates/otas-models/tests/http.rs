//! モデルのアダプタを、ループバックで動かす偽の HTTP サーバに向けて確かめる。
//! 実際の API は呼ばない（API キーもお金も使わない）。

use otas_models::{ModelConfig, build};
use otas_ports::{InferRequest, Message, ModelError, ModelProvider, Role};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

/// 決まった応答を順に返す偽のサーバ。受け取った要求を記録する。
async fn mock(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let log = captured.clone();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_end, content_length) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while buf.len() < head_end + content_length {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let mut lines = head.lines();
            let path = lines
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            let headers = lines
                .filter_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
                })
                .collect();
            let request_body = serde_json::from_slice(&buf[head_end..head_end + content_length])
                .unwrap_or(Value::Null);
            log.lock().unwrap().push(Captured {
                path,
                headers,
                body: request_body,
            });
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    (format!("http://{addr}"), captured)
}

fn config(text: &str) -> ModelConfig {
    toml::from_str(text).unwrap()
}

fn secrets(name: &str) -> Option<String> {
    (name == "TEST_KEY").then(|| "sk-test-secret".to_string())
}

fn request() -> InferRequest {
    InferRequest {
        messages: vec![
            Message {
                role: Role::System,
                content: "簡潔に答える".into(),
            },
            Message {
                role: Role::User,
                content: "こんにちは".into(),
            },
        ],
        max_tokens: 16000,
    }
}

fn claude(base: &str) -> Arc<dyn ModelProvider> {
    build(
        &config(&format!(
            "id = \"claude\"\nprovider = \"anthropic\"\nbase_url = \"{base}\"\napi_key_env = \"TEST_KEY\"\n"
        )),
        &secrets,
    )
    .unwrap()
}

#[tokio::test]
async fn claude_requests_follow_the_messages_api() {
    let (base, log) = mock(vec![(
        200,
        json!({
            "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "やあ"}]
        })
        .to_string(),
    )])
    .await;
    let reply = claude(&base).infer(request()).await.unwrap();
    assert_eq!(reply.content, "やあ");

    let req = log.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.headers["x-api-key"], "sk-test-secret");
    assert_eq!(req.headers["anthropic-version"], "2026-10-21");
    assert_eq!(
        req.headers["anthropic-beta"],
        "server-side-fallback-2026-07-01"
    );
    assert_eq!(req.body["model"], "claude-opus-5-5");
    assert_eq!(req.body["max_tokens"], 16000);
    assert_eq!(req.body["system"], "簡潔に答える");
    assert_eq!(
        req.body["messages"],
        json!([{"role": "user", "content": "こんにちは"}])
    );
    assert_eq!(req.body["output_config"]["effort"], "high");
    assert_eq!(req.body["fallbacks"], "default");
    // サンプリングのパラメータ（2026-10-21 では送ると 400）は送らない
    assert!(req.body.get("temperature").is_none());
}

#[tokio::test]
async fn claude_refusals_are_reported_as_refusals() {
    let (base, _) = mock(vec![(
        200,
        json!({"stop_reason": "refusal", "stop_details": {"category": "bio"}, "content": []})
            .to_string(),
    )])
    .await;
    let err = claude(&base).infer(request()).await.unwrap_err();
    assert!(matches!(err, ModelError::Refused(c) if c == "bio"));
}

#[tokio::test]
async fn overload_is_retried_and_auth_errors_are_not() {
    let ok =
        json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "ok"}]}).to_string();
    let overloaded =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}})
            .to_string();
    let (base, log) = mock(vec![(529, overloaded), (200, ok)]).await;
    let reply = claude(&base).infer(request()).await.unwrap();
    assert_eq!(reply.content, "ok");
    assert_eq!(log.lock().unwrap().len(), 2);

    let unauthorized =
        json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})
            .to_string();
    let (base, log) = mock(vec![(401, unauthorized)]).await;
    let err = claude(&base).infer(request()).await.unwrap_err();
    assert_eq!(log.lock().unwrap().len(), 1);
    let text = err.to_string();
    assert!(matches!(err, ModelError::Failed(_)));
    assert!(text.contains("authentication_error"), "{text}");
    // エラーに API キーは含めない
    assert!(!text.contains("sk-test-secret"));
}

#[tokio::test]
async fn openai_compatible_servers_get_chat_completions() {
    let (base, log) = mock(vec![(
        200,
        json!({"choices": [{"message": {"role": "assistant", "content": "ローカルです"}, "finish_reason": "stop"}]})
            .to_string(),
    )])
    .await;
    let local = build(
        &config(&format!(
            "id = \"local\"\nprovider = \"openai_compatible\"\nmodel = \"llama3\"\nbase_url = \"{base}/v1\"\nlocal = true\n"
        )),
        &secrets,
    )
    .unwrap();
    assert!(local.caps().local);
    let reply = local.infer(request()).await.unwrap();
    assert_eq!(reply.content, "ローカルです");

    let req = log.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/v1/chat/completions");
    assert!(!req.headers.contains_key("authorization"));
    assert_eq!(req.body["model"], "llama3");
    assert_eq!(req.body["messages"][0]["role"], "system");
    assert_eq!(req.body["messages"][1]["content"], "こんにちは");
}

#[tokio::test]
async fn openai_compatible_keys_go_in_the_bearer_header() {
    let (base, log) = mock(vec![(
        200,
        json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]}).to_string(),
    )])
    .await;
    let remote = build(
        &config(&format!(
            "id = \"r\"\nprovider = \"openai_compatible\"\nmodel = \"m\"\nbase_url = \"{base}/v1\"\napi_key_env = \"TEST_KEY\"\n"
        )),
        &secrets,
    )
    .unwrap();
    assert!(!remote.caps().local);
    remote.infer(request()).await.unwrap();
    assert_eq!(
        log.lock().unwrap()[0].headers["authorization"],
        "Bearer sk-test-secret"
    );
}
