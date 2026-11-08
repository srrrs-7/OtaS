//! OtaS の CLI（人間向けの窓口）。
//!
//! ```text
//! otas [--socket <パス>]                                対話モード（OTAS_MODEL のモデルを使う）
//! otas [--socket <パス>] chat [対話モードの指定]          対話モード
//! otas [--socket <パス>] submit <依頼の JSON ファイル>   タスクを依頼する
//! otas [--socket <パス>] tasks                          タスクの一覧
//! otas [--socket <パス>] pending                        承認待ちの一覧
//! otas [--socket <パス>] approve <承認待ちの ID>         承認して実行する
//! otas [--socket <パス>] reject <承認待ちの ID>          却下する
//! otas [--socket <パス>] diff <タスクの ID>              作業領域の変更を見る
//! otas [--socket <パス>] commit <タスクの ID>            変更を正本にコミットする
//! otas [--socket <パス>] discard <タスクの ID>           変更を捨てる
//! otas [--socket <パス>] versions                       正本の版の一覧
//! otas [--socket <パス>] rollback <版>                  正本を指定した版に戻す
//! ```
//!
//! ソケットは `--socket` か、環境変数 `OTAS_SOCKET` で指定する。
//!
//! # 対話モード
//!
//! 入力した依頼を、標準のエージェント（`otas-agent`）のタスクとしてデーモンに依頼し、経過を表示する。
//! 処理は自動で進み、人間に確かめるのは次の2つだけ。
//!
//! - ブローカーが承認待ちにした操作（承認するとエージェントがその結果を受け取って続ける）
//! - 作業領域の変更を正本にコミットするか
//!
//! 対話モードの指定：
//!
//! ```text
//! --model <ID>     使うモデル（省略時は環境変数 OTAS_MODEL）
//! --read <パス>    読める場所（/data 以下。複数指定できる）
//! --write <パス>   書ける場所（/data 以下。複数指定できる）。--read も --write もなければ /data に書ける
//! --tool <名前>    呼べるツール（末尾の * で前方一致。複数指定できる）
//! --agent <パス>   エージェントのプログラム（省略時は OTAS_AGENT、なければ /usr/local/bin/otas-agent）
//! ```

use std::process::ExitCode;

const USAGE: &str = "使い方: otas [--socket <パス>] [chat [--model <ID>] [--read <パス>]... [--write <パス>]... [--tool <名前>]... | submit <ファイル> | tasks | pending | approve <ID> | reject <ID> | diff <タスク> | commit <タスク> | discard <タスク> | versions | rollback <版>]";

#[cfg(unix)]
fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = if args.first().map(String::as_str) == Some("--socket") {
        if args.len() < 2 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        let path = args.remove(1);
        args.remove(0);
        Some(path)
    } else {
        std::env::var("OTAS_SOCKET").ok()
    };
    let Some(socket) = socket else {
        eprintln!("otas: ソケットを --socket か環境変数 OTAS_SOCKET で指定してください");
        return ExitCode::from(2);
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("otas: 実行環境を作れません: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::task::LocalSet::new().block_on(&runtime, async move {
        match run(&socket, &args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("otas: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("otas: 現在は Unix 専用です");
    ExitCode::FAILURE
}

#[cfg(unix)]
async fn run(socket: &str, args: &[String]) -> Result<ExitCode, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // 対話モードの指定は、接続する前に確かめる
    let chat_options = match args.as_slice() {
        [] => Some(chat::Options::parse(&[])?),
        ["chat", rest @ ..] => Some(chat::Options::parse(rest)?),
        _ => None,
    };

    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("{socket} に接続できません: {e}"))?;
    let console = otas_ipc::connect_console(stream);

    if let Some(options) = chat_options {
        return chat::run(&console, options).await;
    }
    match commands::run(&console, &args).await? {
        Some(code) => Ok(code),
        None => {
            eprintln!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

/// サブコマンド（対話モードでは `/` を付けて使う）。
#[cfg(unix)]
mod commands {
    use otas_ipc::{ConsoleClient, Remote};
    use otas_types::{ApprovalId, TaskId};
    use std::process::ExitCode;

    pub(crate) fn parse_task(id: &str) -> Result<TaskId, String> {
        uuid::Uuid::parse_str(id)
            .map(TaskId)
            .map_err(|e| format!("タスクの ID が不正です: {e}"))
    }

    fn parse_approval(id: &str) -> Result<ApprovalId, String> {
        uuid::Uuid::parse_str(id)
            .map(ApprovalId)
            .map_err(|e| format!("承認待ちの ID が不正です: {e}"))
    }

    pub(crate) fn print_changes(changes: &serde_json::Value) {
        for change in changes.as_array().into_iter().flatten() {
            println!(
                "{}\t{}",
                change["kind"].as_str().unwrap_or("?"),
                change["path"].as_str().unwrap_or("?")
            );
        }
    }

    /// コミットの結果を表示する。コミットできたら true。
    pub(crate) fn print_commit(result: &serde_json::Value) -> bool {
        match result["result"].as_str() {
            Some("applied") => {
                println!("コミットしました（版 {}）", result["version"]);
                true
            }
            Some("conflict") => {
                println!("衝突したのでコミットしませんでした: {}", result["paths"]);
                false
            }
            _ => {
                println!("変更はありませんでした");
                true
            }
        }
    }

    /// サブコマンドを実行する。知らないコマンドなら `None`。
    pub(crate) async fn run(
        console: &ConsoleClient,
        args: &[&str],
    ) -> Result<Option<ExitCode>, String> {
        let err = |e: otas_ipc::IpcError| e.to_string();
        match args {
            ["submit", file] => {
                let request = std::fs::read_to_string(file)
                    .map_err(|e| format!("{file} を読めません: {e}"))?;
                let task = console.submit(&request).await.map_err(err)?;
                println!("{}", task.0);
            }
            ["tasks"] => {
                for (id, status) in console.tasks().await.map_err(err)? {
                    println!("{}\t{}", id.0, status);
                }
            }
            ["pending"] => {
                for item in console.approver().pending().await.map_err(err)? {
                    let action = serde_json::to_string(&item.action).unwrap_or_default();
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.id.0, item.task.0, action, item.reason
                    );
                }
            }
            ["approve", id] => {
                let result = console
                    .approver()
                    .approve(parse_approval(id)?)
                    .await
                    .map_err(err)?;
                match result {
                    Remote::Done(value) => println!("実行しました: {value}"),
                    Remote::Pending(next) => println!("まだ承認待ちです: {}", next.0),
                    Remote::Denied(reason) => {
                        println!("拒否されました: {reason}");
                        return Ok(Some(ExitCode::FAILURE));
                    }
                }
            }
            ["reject", id] => {
                console
                    .approver()
                    .reject(parse_approval(id)?)
                    .await
                    .map_err(err)?;
                println!("却下しました");
            }
            ["diff", id] => {
                let changes = console.review(parse_task(id)?).await.map_err(err)?;
                print_changes(&changes);
            }
            ["commit", id] => {
                let result = console.commit(parse_task(id)?).await.map_err(err)?;
                if !print_commit(&result) {
                    return Ok(Some(ExitCode::FAILURE));
                }
            }
            ["discard", id] => {
                console.discard(parse_task(id)?).await.map_err(err)?;
                println!("変更を捨てました");
            }
            ["versions"] => {
                let v = console.versions().await.map_err(err)?;
                println!("現在の版: {}", v["current"]);
                println!("版の一覧: {}", v["versions"]);
            }
            ["rollback", version] => {
                let version: u64 = version
                    .parse()
                    .map_err(|_| format!("版は数字で指定してください: {version}"))?;
                let result = console.rollback(version).await.map_err(err)?;
                println!(
                    "版 {version} に戻しました（新しい版 {}）",
                    result["version"]
                );
            }
            _ => return Ok(None),
        }
        Ok(Some(ExitCode::SUCCESS))
    }
}

/// 対話モード。
#[cfg(unix)]
mod chat {
    use otas_ipc::{ConsoleClient, Remote};
    use otas_types::{Action, ApprovalId, Grant, GrantSet, TaskId};
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::io::Write;
    use std::process::ExitCode;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

    const DEFAULT_AGENT: &str = "/usr/local/bin/otas-agent";
    /// 経過と承認待ちを確かめる間隔。
    const POLL: Duration = Duration::from_millis(200);

    const HELP: &str = "\
依頼を日本語などで入力すると、エージェントが処理します。
承認が必要な操作と、変更のコミットだけを確かめます。

  /tasks              タスクの一覧
  /pending            承認待ちの一覧
  /diff <タスク>      作業領域の変更
  /commit <タスク>    変更をコミットする
  /discard <タスク>   変更を捨てる
  /versions           正本の版の一覧
  /rollback <版>      正本を指定した版に戻す
  /help               この説明
  /exit               終わる（Ctrl-D でも終わる）";

    /// 対話モードの指定。
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct Options {
        model: String,
        agent: String,
        grants: Vec<Grant>,
    }

    impl Options {
        pub(crate) fn parse(args: &[&str]) -> Result<Self, String> {
            Self::parse_with(args, |name| std::env::var(name).ok())
        }

        fn parse_with(args: &[&str], env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
            let mut model = None;
            let mut agent = None;
            let mut data = Vec::new();
            let mut tools = Vec::new();
            let mut it = args.iter();
            while let Some(flag) = it.next() {
                let mut value = || {
                    it.next()
                        .map(|v| v.to_string())
                        .ok_or(format!("{flag} には値を指定してください"))
                };
                match *flag {
                    "--model" => model = Some(value()?),
                    "--agent" => agent = Some(value()?),
                    "--read" => data.push(Grant::Read { prefix: value()? }),
                    "--write" => data.push(Grant::Write { prefix: value()? }),
                    "--tool" => tools.push(Grant::Tool { pattern: value()? }),
                    other => return Err(format!("対話モードの指定が不正です: {other}")),
                }
            }
            let model = model
                .or_else(|| env("OTAS_MODEL"))
                .filter(|m| !m.is_empty())
                .ok_or("使うモデルを --model か環境変数 OTAS_MODEL で指定してください")?;
            let agent = agent
                .or_else(|| env("OTAS_AGENT"))
                .unwrap_or_else(|| DEFAULT_AGENT.into());
            if data.is_empty() {
                data.push(Grant::Write {
                    prefix: "/data".into(),
                });
            }
            let mut grants = data;
            grants.extend(tools);
            grants.push(Grant::Model {
                pattern: model.clone(),
            });
            Ok(Self {
                model,
                agent,
                grants,
            })
        }

        fn request(&self, goal: &str) -> Value {
            json!({
                "program": self.agent,
                "args": ["--model", self.model, "--wait-approvals", goal],
                "grants": GrantSet::new(self.grants.clone()),
            })
        }
    }

    /// 標準入力を1行ずつ読む。
    struct Input(Lines<BufReader<Stdin>>);

    impl Input {
        fn new() -> Self {
            Self(BufReader::new(tokio::io::stdin()).lines())
        }

        /// 問いを表示して1行読む。入力が終わったら `None`。
        async fn ask(&mut self, prompt: &str) -> Result<Option<String>, String> {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            self.0
                .next_line()
                .await
                .map(|l| l.map(|l| l.trim().to_string()))
                .map_err(|e| format!("入力を読めません: {e}"))
        }
    }

    fn err(e: otas_ipc::IpcError) -> String {
        e.to_string()
    }

    pub(crate) async fn run(console: &ConsoleClient, options: Options) -> Result<ExitCode, String> {
        println!("OtaS の対話モードです（/help で説明、/exit で終了）");
        println!(
            "モデル: {}  権限: {}",
            options.model,
            options
                .grants
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、")
        );
        let mut input = Input::new();
        loop {
            let Some(line) = input.ask("\notas> ").await? else {
                println!();
                break;
            };
            if line.is_empty() {
                continue;
            }
            let result = if let Some(command) = line.strip_prefix('/') {
                let words: Vec<&str> = command.split_whitespace().collect();
                match words.as_slice() {
                    ["exit"] | ["quit"] => break,
                    ["help"] => {
                        println!("{HELP}");
                        Ok(())
                    }
                    _ => match crate::commands::run(console, &words).await {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => Err(format!("不明なコマンドです: /{command}（/help で一覧）")),
                        Err(e) => Err(e),
                    },
                }
            } else {
                turn(console, &options, &line, &mut input).await
            };
            if let Err(e) = result {
                println!("エラー: {e}");
            }
        }
        Ok(ExitCode::SUCCESS)
    }

    /// 1回の依頼。タスクが終わるまで経過を表示し、承認待ちとレビューを人間に確かめる。
    async fn turn(
        console: &ConsoleClient,
        options: &Options,
        goal: &str,
        input: &mut Input,
    ) -> Result<(), String> {
        let task = console
            .submit(&options.request(goal).to_string())
            .await
            .map_err(err)?;
        let approver = console.approver();
        let mut offset = 0;
        let mut asked: HashSet<ApprovalId> = HashSet::new();
        let status = loop {
            offset = show_output(console, task, offset).await?;

            for item in approver.pending().await.map_err(err)? {
                if item.task != task || !asked.insert(item.id) {
                    continue;
                }
                println!("  ⚠ 承認が必要です: {}", describe(&item.action));
                println!("    理由: {}", item.reason);
                let answer = input.ask("    承認しますか？ [y/N] ").await?;
                if matches!(answer.as_deref(), Some("y" | "Y" | "yes")) {
                    match approver.approve(item.id).await {
                        Ok(Remote::Done(_)) => println!("    承認しました"),
                        Ok(Remote::Denied(reason)) => println!("    拒否されました: {reason}"),
                        Ok(Remote::Pending(_)) => println!("    まだ承認待ちです"),
                        Err(e) => println!("    実行に失敗しました: {e}"),
                    }
                } else {
                    approver.reject(item.id).await.map_err(err)?;
                    println!("    却下しました");
                }
            }

            let status = console
                .tasks()
                .await
                .map_err(err)?
                .into_iter()
                .find(|(id, _)| *id == task)
                .map(|(_, s)| s)
                .ok_or("依頼したタスクが見つかりません")?;
            if status["state"] != "running" {
                // 終わる直前に書かれた出力を読み切る
                loop {
                    let next = show_output(console, task, offset).await?;
                    if next == offset {
                        break;
                    }
                    offset = next;
                }
                break status;
            }
            tokio::time::sleep(POLL).await;
        };
        finish(console, task, &status, input).await
    }

    /// 新しい出力を表示し、次に読む位置を返す。
    async fn show_output(
        console: &ConsoleClient,
        task: TaskId,
        offset: u64,
    ) -> Result<u64, String> {
        let (text, next) = console.output(task, offset).await.map_err(err)?;
        for line in text.lines() {
            if let Some(shown) = render(line) {
                println!("{shown}");
            }
        }
        Ok(next)
    }

    /// エージェントの経過の1行（JSON）を、人間に見せる形にする。見せないものは `None`。
    pub(crate) fn render(line: &str) -> Option<String> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Some(line.to_string());
        };
        let text = |key: &str| event[key].as_str().unwrap_or("").trim().to_string();
        match (event["kind"].as_str()?, event["result"].as_str()) {
            // ツールを選んだときの説明。ツールを呼ばない応答は final で表示する
            ("model", Some("done")) => {
                let calls = event["tool_calls"].as_array().map_or(0, Vec::len);
                let t = text("text");
                (calls > 0 && !t.is_empty()).then(|| format!("  {t}"))
            }
            ("model", Some("pending")) => {
                Some("  モデルへの送信が承認待ちになり、エージェントは止まりました".into())
            }
            ("tool", Some(result)) => {
                let note = match result {
                    "done" => "",
                    "pending" => "（承認待ち。まだ実行されていません）",
                    "denied" => "（拒否されました）",
                    "unknown" => "（そのツールはありません）",
                    _ => "（失敗しました）",
                };
                Some(format!("  ▸ {}{note}", text("name")))
            }
            ("final", _) => Some(format!("\n{}", text("text"))),
            ("error", _) => Some(format!("  エラー: {}", text("message"))),
            _ => None,
        }
    }

    fn describe(action: &Action) -> String {
        match action {
            Action::CallTool { tool } => format!("ツール {tool} の呼び出し"),
            Action::Infer { model } => format!("モデル {model} への送信"),
            Action::ReadData { path } => format!("{path} の読み取り"),
            Action::WriteData { path } => format!("{path} への書き込み"),
        }
    }

    /// タスクが終わった後の処理。変更があれば、コミットするかを確かめる。
    async fn finish(
        console: &ConsoleClient,
        task: TaskId,
        status: &Value,
        input: &mut Input,
    ) -> Result<(), String> {
        match status["state"].as_str() {
            Some("awaiting_review") => {
                let changes = console.review(task).await.map_err(err)?;
                println!(
                    "\n変更が {} 件あります（まだ正本には反映していません）",
                    status["changes"]
                );
                crate::commands::print_changes(&changes);
                let answer = input
                    .ask("コミットしますか？ [c] コミット / [x] 捨てる / [Enter] あとで ")
                    .await?;
                match answer.as_deref() {
                    Some("c" | "C") => {
                        let result = console.commit(task).await.map_err(err)?;
                        crate::commands::print_commit(&result);
                    }
                    Some("x" | "X") => {
                        console.discard(task).await.map_err(err)?;
                        println!("変更を捨てました");
                    }
                    _ => println!(
                        "あとで /commit {} か /discard {} で決めてください",
                        task.0, task.0
                    ),
                }
            }
            Some("exited") => {
                if status["code"] != 0 {
                    println!(
                        "エージェントが終了コード {} で終わりました（タスク {}）",
                        status["code"], task.0
                    );
                }
            }
            _ => println!("タスク {} の状態: {status}", task.0),
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn options_default_to_writing_data_with_the_model_from_the_environment() {
            let env = |name: &str| (name == "OTAS_MODEL").then(|| "claude".to_string());
            let o = Options::parse_with(&[], env).unwrap();
            assert_eq!(o.model, "claude");
            assert_eq!(o.agent, DEFAULT_AGENT);
            assert_eq!(
                o.grants,
                vec![
                    Grant::Write {
                        prefix: "/data".into()
                    },
                    Grant::Model {
                        pattern: "claude".into()
                    }
                ]
            );
            assert_eq!(
                o.request("こんにちは")["args"],
                json!(["--model", "claude", "--wait-approvals", "こんにちは"])
            );
        }

        #[test]
        fn options_narrow_grants_and_require_a_model() {
            let none = |_: &str| None;
            let o = Options::parse_with(
                &[
                    "--model",
                    "local",
                    "--read",
                    "/data/mail",
                    "--tool",
                    "web.*",
                ],
                none,
            )
            .unwrap();
            assert_eq!(
                o.grants,
                vec![
                    Grant::Read {
                        prefix: "/data/mail".into()
                    },
                    Grant::Tool {
                        pattern: "web.*".into()
                    },
                    Grant::Model {
                        pattern: "local".into()
                    }
                ]
            );
            assert!(Options::parse_with(&[], none).is_err());
            assert!(Options::parse_with(&["--model"], none).is_err());
            assert!(Options::parse_with(&["--model", "m", "--bogus"], none).is_err());
        }

        #[test]
        fn events_are_rendered_for_people() {
            assert_eq!(
                render(r#"{"kind":"tool","name":"fs__write","result":"done"}"#).unwrap(),
                "  ▸ fs__write"
            );
            assert_eq!(
                render(r#"{"kind":"final","text":"終わりました"}"#).unwrap(),
                "\n終わりました"
            );
            // ツールを呼ばない応答は final で表示するので、ここでは出さない
            assert!(
                render(r#"{"kind":"model","result":"done","text":"終わりました","tool_calls":[]}"#)
                    .is_none()
            );
            assert!(render(r#"{"kind":"approval","result":"waiting"}"#).is_none());
        }
    }
}
