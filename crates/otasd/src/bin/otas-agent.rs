//! OtaS の標準のエージェント。
//!
//! 使い方：`otas-agent --model <モデルの ID> [--max-steps N] [--max-tokens N] [--wait-approvals] [--history <JSON>] [--] <依頼>`
//! （`otasd` がサンドボックスの中で起動する）
//!
//! モデルにツールを選ばせて実行し、結果をモデルに返すことを、モデルが終わるまで繰り返す。
//!
//! - ツールは2種類：
//!   - ブローカーのツール（権限のあるものだけ）。呼び出しはすべて判定され、監査ログに残る
//!   - 作業領域のファイル操作（`fs__read`・`fs__write`・`fs__list`）。サンドボックスの中で直接行う
//! - モデルの呼び出しもブローカーを通る。私的データと信頼できない入力の両方に触れた後は、
//!   クラウドのモデルへの送信が承認待ちになり、エージェントはそこで止まる
//! - 承認待ちになったツールの呼び出しは、まだ実行されていないことをモデルに伝える
//! - `--wait-approvals` を付けると、承認待ちで止まらずに人間の判断を待ち、承認されたら結果を受け取って続ける
//!   （人間が画面の前にいるとき、たとえば `otas` の対話モードで使う）
//! - `--history` で前のやりとりを渡すと、会話の続きとして扱う。形は
//!   `[{"role": "user", "text": "..."}, {"role": "assistant", "text": "..."}, ...]`
//!   （前のタスクのラベルは、依頼の `continues` でデーモンが引き継ぐ）
//! - 経過は1行1件の JSON で標準出力に書く（監査とテストのため）

use std::process::ExitCode;

/// 作業領域のファイル操作の名前（モデルに見せる名前）。
const FS_READ: &str = "fs__read";
const FS_WRITE: &str = "fs__write";
const FS_LIST: &str = "fs__list";

/// 読み込むファイルの上限（バイト）。超えた分は切り捨ててモデルに伝える。
const READ_LIMIT: usize = 256 * 1024;

const SYSTEM_PROMPT: &str = "あなたは OtaS のサンドボックスの中で動くエージェントです。\
ユーザーの依頼を、与えられたツールを使ってやり遂げてください。\n\
- ファイルは fs__read・fs__write・fs__list で扱います。パスは作業領域の根（/data）からの相対パスです。\
書いた内容は人間がレビューしてから正本に反映されます。\n\
- それ以外のツールは OS のブローカーを通って実行されます。結果が「承認待ち」の操作はまだ実行されていません。\n\
- 終わったら、何をしたか、承認やレビューが必要なものは何かを短く報告してください。";

#[derive(Debug)]
struct Options {
    model: String,
    max_steps: usize,
    max_tokens: u32,
    wait_approvals: bool,
    history: Vec<Turn>,
    goal: String,
}

/// 前のやりとりの1件。
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    role: Speaker,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Speaker {
    User,
    Assistant,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut model = None;
    let mut max_steps = 20;
    let mut max_tokens = 16000;
    let mut wait_approvals = false;
    let mut history = Vec::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                model = args.get(i + 1).cloned();
                i += 2;
            }
            "--max-steps" => {
                max_steps = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--max-steps には数を指定してください")?;
                i += 2;
            }
            "--max-tokens" => {
                max_tokens = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--max-tokens には数を指定してください")?;
                i += 2;
            }
            "--wait-approvals" => {
                wait_approvals = true;
                i += 1;
            }
            "--history" => {
                let json = args
                    .get(i + 1)
                    .ok_or("--history には JSON を指定してください")?;
                history = serde_json::from_str(json)
                    .map_err(|e| format!("--history の形式が不正です: {e}"))?;
                i += 2;
            }
            // これより後はすべて依頼（依頼が - で始まっていても指定と取り違えない）
            "--" => {
                rest.extend(args[i + 1..].iter().cloned());
                break;
            }
            other => {
                rest.push(other.to_string());
                i += 1;
            }
        }
    }
    let model = model.ok_or("--model でモデルの ID を指定してください")?;
    if rest.is_empty() {
        return Err("依頼を指定してください".into());
    }
    Ok(Options {
        model,
        max_steps,
        max_tokens,
        wait_approvals,
        history,
        goal: rest.join(" "),
    })
}

#[cfg(unix)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("otas-agent: {e}");
            eprintln!(
                "使い方: otas-agent --model <ID> [--max-steps N] [--max-tokens N] [--wait-approvals] [--history <JSON>] [--] <依頼>"
            );
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("otas-agent: 実行環境を作れません: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::task::LocalSet::new().block_on(&runtime, agent::run(options))
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("otas-agent: 現在は Unix 専用です");
    ExitCode::FAILURE
}

#[cfg(unix)]
mod agent {
    use super::*;
    use otas_ipc::{Remote, TaskClient};
    use otas_ports::{
        InferRequest, Message, Role, StopReason, ToolCall, ToolDef, ToolResult, ToolSpec,
    };
    use otas_types::ApprovalId;
    use serde_json::{Value, json};
    use std::path::{Component, Path, PathBuf};

    fn emit(line: Value) {
        println!("{line}");
    }

    /// ブローカーのツール名を、モデルに見せる名前にする（`.` はプロバイダの制約で使えない）。
    fn model_name(tool: &str) -> String {
        tool.replace('.', "__")
    }

    fn fs_tools() -> Vec<ToolDef> {
        let path = json!({"type": "string", "description": "作業領域の根（/data）からの相対パス"});
        vec![
            ToolDef {
                name: FS_READ.into(),
                description: "作業領域のファイルを読む".into(),
                input_schema: json!({"type": "object", "properties": {"path": path}, "required": ["path"]}),
            },
            ToolDef {
                name: FS_WRITE.into(),
                description:
                    "作業領域のファイルに書く（上書き。人間のレビュー後に正本に反映される）".into(),
                input_schema: json!({"type": "object",
                    "properties": {"path": path, "content": {"type": "string"}},
                    "required": ["path", "content"]}),
            },
            ToolDef {
                name: FS_LIST.into(),
                description: "作業領域のディレクトリの中身を一覧する".into(),
                input_schema: json!({"type": "object", "properties": {"path": path}}),
            },
        ]
    }

    /// 相対パスを作業領域の中のパスにする。`..` や絶対パスは使えない。
    fn resolve(root: &Path, input: &Value) -> Result<PathBuf, String> {
        let rel = input["path"].as_str().unwrap_or("");
        let rel = Path::new(rel);
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(format!(
                "パスは作業領域からの相対パスで指定してください: {}",
                rel.display()
            ));
        }
        Ok(root.join(rel))
    }

    fn run_fs(root: &Path, call: &ToolCall) -> Result<String, String> {
        match call.name.as_str() {
            FS_READ => {
                let path = resolve(root, &call.input)?;
                let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                let mut text =
                    String::from_utf8_lossy(&bytes[..bytes.len().min(READ_LIMIT)]).to_string();
                if bytes.len() > READ_LIMIT {
                    text.push_str(&format!(
                        "\n（{} バイトのうち先頭だけを表示しました）",
                        bytes.len()
                    ));
                }
                Ok(text)
            }
            FS_WRITE => {
                let path = resolve(root, &call.input)?;
                let content = call.input["content"].as_str().ok_or("content が必要です")?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, content).map_err(|e| e.to_string())?;
                Ok(format!("{} バイト書きました", content.len()))
            }
            _ => {
                let path = if call.input["path"].is_null() {
                    root.to_path_buf()
                } else {
                    resolve(root, &call.input)?
                };
                let mut names: Vec<String> = std::fs::read_dir(&path)
                    .map_err(|e| e.to_string())?
                    .filter_map(|e| {
                        let e = e.ok()?;
                        let dir = e.file_type().ok()?.is_dir();
                        Some(format!(
                            "{}{}",
                            e.file_name().to_string_lossy(),
                            if dir { "/" } else { "" }
                        ))
                    })
                    .collect();
                names.sort();
                Ok(names.join("\n"))
            }
        }
    }

    /// 承認待ちを知らせ、人間の判断が出るまで待つ。
    async fn wait_for(client: &TaskClient, id: ApprovalId) -> Result<Remote<Value>, String> {
        emit(json!({"kind": "approval", "approval": id.0, "result": "waiting"}));
        let outcome = client
            .wait_approval(id)
            .await
            .map_err(|e| format!("承認の結果を受け取れません: {e}"));
        let result = match &outcome {
            Ok(Remote::Done(_)) => "approved",
            Ok(_) => "rejected",
            Err(_) => "error",
        };
        emit(json!({"kind": "approval", "approval": id.0, "result": result}));
        outcome
    }

    async fn run_remote(
        client: &TaskClient,
        tool: &ToolSpec,
        call: &ToolCall,
        wait: bool,
    ) -> ToolResult {
        let mut outcome = client.call_tool(&tool.name, &call.input).await;
        if wait && let Ok(Remote::Pending(id)) = outcome {
            outcome = match wait_for(client, id).await {
                Ok(r) => Ok(r),
                Err(e) => {
                    return ToolResult {
                        call_id: call.id.clone(),
                        content: e,
                        is_error: true,
                    };
                }
            };
        }
        let (content, is_error, result) = match outcome {
            Ok(Remote::Done(v)) => (v.to_string(), false, "done"),
            Ok(Remote::Pending(id)) => (
                format!(
                    "承認待ちです（ID {}）。この操作はまだ実行されていません。人間が承認すると実行されます。",
                    id.0
                ),
                false,
                "pending",
            ),
            Ok(Remote::Denied(reason)) => (format!("拒否されました: {reason}"), true, "denied"),
            Err(e) => (format!("失敗しました: {e}"), true, "error"),
        };
        emit(json!({"kind": "tool", "name": tool.name, "result": result}));
        ToolResult {
            call_id: call.id.clone(),
            content,
            is_error,
        }
    }

    pub(super) async fn run(options: Options) -> ExitCode {
        let fail = |message: String| {
            eprintln!("otas-agent: {message}");
            emit(json!({"kind": "error", "message": message}));
            ExitCode::FAILURE
        };
        let client = match TaskClient::from_stdin() {
            Ok(c) => c,
            Err(e) => return fail(format!("窓口に接続できません: {e}")),
        };
        let root = PathBuf::from(std::env::var("OTAS_DATA").unwrap_or_else(|_| "/data".into()));

        let models = match client.models().await {
            Ok(m) => m,
            Err(e) => return fail(format!("モデルの一覧を取得できません: {e}")),
        };
        match models.iter().find(|m| m.id == options.model) {
            Some(m) if m.tool_use => {}
            Some(_) => {
                return fail(format!(
                    "モデル {} はツールの呼び出しに対応していません",
                    options.model
                ));
            }
            None => return fail(format!("モデル {} を使う権限がありません", options.model)),
        }
        let remote = match client.tools().await {
            Ok(t) => t,
            Err(e) => return fail(format!("ツールの一覧を取得できません: {e}")),
        };
        let mut tools = fs_tools();
        tools.extend(remote.iter().map(|t| ToolDef {
            name: model_name(&t.name),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        }));

        let mut messages = vec![Message::text(Role::System, SYSTEM_PROMPT)];
        for turn in &options.history {
            let role = match turn.role {
                Speaker::User => Role::User,
                Speaker::Assistant => Role::Assistant,
            };
            messages.push(Message::text(role, turn.text.clone()));
        }
        messages.push(Message::text(Role::User, options.goal.clone()));

        for step in 0..options.max_steps {
            let request = InferRequest {
                messages: messages.clone(),
                max_tokens: options.max_tokens,
                tools: tools.clone(),
            };
            let mut outcome = client.infer_full(&options.model, &request).await;
            if options.wait_approvals
                && let Ok(Remote::Pending(id)) = outcome
            {
                outcome = match wait_for(&client, id).await {
                    Ok(Remote::Done(v)) => serde_json::from_value(v)
                        .map(Remote::Done)
                        .map_err(otas_ipc::IpcError::from),
                    Ok(Remote::Denied(reason)) => Ok(Remote::Denied(reason)),
                    Ok(Remote::Pending(next)) => Ok(Remote::Pending(next)),
                    Err(e) => return fail(e),
                };
            }
            let response = match outcome {
                Ok(Remote::Done(r)) => r,
                Ok(Remote::Pending(id)) => {
                    emit(
                        json!({"kind": "model", "step": step, "result": "pending", "approval": id.0}),
                    );
                    eprintln!(
                        "otas-agent: モデルへの送信が承認待ちになりました（ID {}）。ここで止まります",
                        id.0
                    );
                    return ExitCode::from(3);
                }
                Ok(Remote::Denied(reason)) => {
                    return fail(format!("モデルの呼び出しを拒否されました: {reason}"));
                }
                Err(e) => return fail(format!("モデルの呼び出しに失敗しました: {e}")),
            };
            emit(json!({
                "kind": "model", "step": step, "result": "done",
                "stop": response.stop_reason, "text": response.content,
                "tool_calls": response.tool_calls.iter().map(|c| &c.name).collect::<Vec<_>>(),
            }));

            let calls = response.tool_calls.clone();
            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_results: Vec::new(),
                provider_raw: response.provider_raw.clone(),
            });

            if calls.is_empty() {
                if response.stop_reason == StopReason::MaxTokens {
                    return fail("応答が最大トークン数で切れました".into());
                }
                emit(json!({"kind": "final", "text": response.content}));
                return ExitCode::SUCCESS;
            }

            let mut results = Vec::new();
            for call in &calls {
                let result = if call.name.starts_with("fs__") {
                    let outcome = run_fs(&root, call);
                    emit(json!({"kind": "tool", "name": call.name,
                                "result": if outcome.is_ok() { "done" } else { "error" }}));
                    match outcome {
                        Ok(content) => ToolResult {
                            call_id: call.id.clone(),
                            content,
                            is_error: false,
                        },
                        Err(content) => ToolResult {
                            call_id: call.id.clone(),
                            content,
                            is_error: true,
                        },
                    }
                } else if let Some(tool) = remote.iter().find(|t| model_name(&t.name) == call.name)
                {
                    run_remote(&client, tool, call, options.wait_approvals).await
                } else {
                    emit(json!({"kind": "tool", "name": call.name, "result": "unknown"}));
                    ToolResult {
                        call_id: call.id.clone(),
                        content: format!("ツール {} はありません", call.name),
                        is_error: true,
                    }
                };
                results.push(result);
            }
            messages.push(Message {
                role: Role::User,
                tool_results: results,
                ..Message::default()
            });
        }
        fail(format!("{} 回で終わりませんでした", options.max_steps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn history_and_the_goal_after_the_separator_are_parsed() {
        let o = parse_args(&args(&[
            "--model",
            "claude",
            "--history",
            r#"[{"role":"user","text":"前の依頼"},{"role":"assistant","text":"前の応答"}]"#,
            "--",
            "--model",
            "の意味は？",
        ]))
        .unwrap();
        assert_eq!(o.model, "claude");
        assert_eq!(
            o.history,
            vec![
                Turn {
                    role: Speaker::User,
                    text: "前の依頼".into()
                },
                Turn {
                    role: Speaker::Assistant,
                    text: "前の応答".into()
                },
            ]
        );
        assert_eq!(o.goal, "--model の意味は？");
    }

    #[test]
    fn a_malformed_history_is_rejected() {
        for bad in [r#"[{"role":"system","text":"x"}]"#, "{}", "["] {
            assert!(
                parse_args(&args(&["--model", "m", "--history", bad, "依頼"])).is_err(),
                "{bad}"
            );
        }
    }
}
