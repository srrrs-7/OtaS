//! 動作確認用のエージェント。
//!
//! 使い方：`otas-demo-agent '<手順の JSON 配列>'`（`otasd` がサンドボックスで起動する）
//!
//! 手順の例：
//!
//! ```json
//! [
//!   {"tool": "demo.read_private"},
//!   {"tool": "demo.send", "args": {"to": "someone@example.com"}},
//!   {"read_file": "$OTAS_DATA/mail/1.eml"},
//!   {"write_file": "$OTAS_DATA/projects/a/notes.md", "content": "v2"},
//!   {"read_file": "/etc/passwd"},
//!   {"infer": "local", "prompt": "要約して"}
//! ]
//! ```
//!
//! ツールは標準入力として渡されたタスク専用の窓口（ブローカー）経由で呼ぶ。
//! `read_file` と `write_file` はファイルを直接読み書きする（作業領域とサンドボックスの確認用）。
//! パスの先頭の `$OTAS_DATA` は作業領域の根（正本の `/data` に当たる）に置き換える。
//! 結果は1手順につき1行の JSON で標準出力に書く。

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 先頭の `$OTAS_DATA` を作業領域の根に置き換える。
fn expand(path: &Path) -> PathBuf {
    match (path.strip_prefix("$OTAS_DATA"), std::env::var("OTAS_DATA")) {
        (Ok(rest), Ok(root)) => Path::new(&root).join(rest),
        _ => path.to_path_buf(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Step {
    Tool {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    ReadFile {
        read_file: PathBuf,
    },
    WriteFile {
        write_file: PathBuf,
        content: String,
    },
    Infer {
        infer: String,
        prompt: String,
        #[serde(default)]
        max_tokens: Option<u32>,
    },
}

#[cfg(unix)]
fn main() -> ExitCode {
    let Some(steps) = std::env::args().nth(1) else {
        eprintln!("使い方: otas-demo-agent '<手順の JSON 配列>'");
        return ExitCode::from(2);
    };
    let steps: Vec<Step> = match serde_json::from_str(&steps) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("otas-demo-agent: 手順を読めません: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("otas-demo-agent: 実行環境を作れません: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::task::LocalSet::new().block_on(&runtime, run(steps))
}

#[cfg(unix)]
async fn run(steps: Vec<Step>) -> ExitCode {
    use otas_ipc::{Remote, TaskClient};
    use serde_json::json;

    let client = match TaskClient::from_stdin() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("otas-demo-agent: 窓口に接続できません: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (i, step) in steps.into_iter().enumerate() {
        let line = match step {
            Step::Tool { tool, args } => match client.call_tool(&tool, &args).await {
                Ok(Remote::Done(value)) => {
                    json!({"step": i, "tool": tool, "result": "done", "value": value})
                }
                Ok(Remote::Pending(id)) => {
                    json!({"step": i, "tool": tool, "result": "pending", "approval": id.0})
                }
                Ok(Remote::Denied(reason)) => {
                    json!({"step": i, "tool": tool, "result": "denied", "reason": reason})
                }
                Err(e) => {
                    json!({"step": i, "tool": tool, "result": "error", "error": e.to_string()})
                }
            },
            Step::Infer {
                infer,
                prompt,
                max_tokens,
            } => {
                let request = otas_ports::InferRequest {
                    messages: vec![otas_ports::Message::text(otas_ports::Role::User, prompt)],
                    max_tokens: max_tokens.unwrap_or(1024),
                    ..Default::default()
                };
                match client.infer(&infer, &request).await {
                    Ok(Remote::Done(text)) => {
                        json!({"step": i, "infer": infer, "result": "done", "text": text})
                    }
                    Ok(Remote::Pending(id)) => {
                        json!({"step": i, "infer": infer, "result": "pending", "approval": id.0})
                    }
                    Ok(Remote::Denied(reason)) => {
                        json!({"step": i, "infer": infer, "result": "denied", "reason": reason})
                    }
                    Err(e) => {
                        json!({"step": i, "infer": infer, "result": "error", "error": e.to_string()})
                    }
                }
            }
            Step::WriteFile {
                write_file,
                content,
            } => match std::fs::write(expand(&write_file), content) {
                Ok(()) => json!({"step": i, "write_file": write_file, "result": "done"}),
                Err(e) => {
                    json!({"step": i, "write_file": write_file, "result": "error", "error": e.to_string()})
                }
            },
            Step::ReadFile { read_file } => match std::fs::read(expand(&read_file)) {
                Ok(bytes) => {
                    json!({"step": i, "read_file": read_file, "result": "done", "bytes": bytes.len()})
                }
                Err(e) => {
                    json!({"step": i, "read_file": read_file, "result": "error", "error": e.to_string()})
                }
            },
        };
        println!("{line}");
    }
    ExitCode::SUCCESS
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("otas-demo-agent: 現在は Unix 専用です");
    ExitCode::FAILURE
}
