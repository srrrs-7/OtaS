//! OtaS の CLI（人間向けの窓口）。
//!
//! ```text
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

use std::process::ExitCode;

const USAGE: &str = "使い方: otas [--socket <パス>] <submit <ファイル> | tasks | pending | approve <ID> | reject <ID> | diff <タスク> | commit <タスク> | discard <タスク> | versions | rollback <版>>";

#[cfg(unix)]
fn parse_task(id: &str) -> Result<otas_types::TaskId, String> {
    uuid::Uuid::parse_str(id)
        .map(otas_types::TaskId)
        .map_err(|e| format!("タスクの ID が不正です: {e}"))
}

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

#[cfg(unix)]
fn parse_approval(id: &str) -> Result<otas_types::ApprovalId, String> {
    uuid::Uuid::parse_str(id)
        .map(otas_types::ApprovalId)
        .map_err(|e| format!("承認待ちの ID が不正です: {e}"))
}

#[cfg(unix)]
async fn run(socket: &str, args: &[String]) -> Result<ExitCode, String> {
    use otas_ipc::{Remote, connect_console};

    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("{socket} に接続できません: {e}"))?;
    let console = connect_console(stream);
    let err = |e: otas_ipc::IpcError| e.to_string();

    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["submit", file] => {
            let request =
                std::fs::read_to_string(file).map_err(|e| format!("{file} を読めません: {e}"))?;
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
                    return Ok(ExitCode::FAILURE);
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
            for change in changes.as_array().into_iter().flatten() {
                println!(
                    "{}\t{}",
                    change["kind"].as_str().unwrap_or("?"),
                    change["path"].as_str().unwrap_or("?")
                );
            }
        }
        ["commit", id] => {
            let result = console.commit(parse_task(id)?).await.map_err(err)?;
            match result["result"].as_str() {
                Some("applied") => println!("コミットしました（版 {}）", result["version"]),
                Some("conflict") => {
                    println!("衝突したのでコミットしませんでした: {}", result["paths"]);
                    return Ok(ExitCode::FAILURE);
                }
                _ => println!("変更はありませんでした"),
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
        _ => {
            eprintln!("{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("otas: 現在は Unix 専用です");
    ExitCode::FAILURE
}
