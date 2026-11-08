//! HTTP の共通処理（クライアントの作成と、再試行）。

use crate::ModelConfigError;
use otas_ports::ModelError;
use reqwest::StatusCode;
use std::time::Duration;

/// 再試行の回数（初回を除く）。
pub(crate) const MAX_RETRIES: u32 = 2;

pub(crate) fn client(id: &str, timeout: Duration) -> Result<reqwest::Client, ModelConfigError> {
    reqwest::Client::builder()
        .timeout(timeout)
        // API キーのヘッダを別のホストに送らないよう、リダイレクトはたどらない
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ModelConfigError::Invalid {
            id: id.to_string(),
            reason: format!("HTTP クライアントを作れません: {e}"),
        })
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    match retry_after {
        Some(secs) => Duration::from_secs(secs.min(30)),
        None => Duration::from_millis(500 * 2u64.pow(attempt)),
    }
}

/// 要求を送り、429・5xx・接続エラーなら待ってから再試行する。最終的な状態コードと本文を返す。
pub(crate) async fn send(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<(StatusCode, String), ModelError> {
    let mut attempt = 0;
    loop {
        match build().send().await {
            Err(e) => {
                if attempt < MAX_RETRIES {
                    tokio::time::sleep(delay(attempt, None)).await;
                    attempt += 1;
                    continue;
                }
                // URL は含むが、ヘッダ（API キー）は含まない
                return Err(ModelError::Unavailable(e.without_url().to_string()));
            }
            Ok(response) => {
                let status = response.status();
                if retryable(status) && attempt < MAX_RETRIES {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse().ok());
                    tokio::time::sleep(delay(attempt, retry_after)).await;
                    attempt += 1;
                    continue;
                }
                let body = response
                    .text()
                    .await
                    .map_err(|e| ModelError::Unavailable(e.without_url().to_string()))?;
                return Ok((status, body));
            }
        }
    }
}

/// エラー応答を、再試行で解消しうるもの（Unavailable）とそうでないもの（Failed）に分ける。
pub(crate) fn error(status: StatusCode, body: &str) -> ModelError {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            let kind = e.get("type").and_then(|t| t.as_str()).unwrap_or("error");
            let message = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            Some(format!("{kind}: {message}"))
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    let text = format!("HTTP {} {}", status.as_u16(), detail);
    if retryable(status) {
        ModelError::Unavailable(text)
    } else {
        ModelError::Failed(text)
    }
}
