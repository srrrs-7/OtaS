//! エンドツーエンドのテスト。
//!
//! デーモン（監督・ブローカー・人間向けの窓口）を動かし、動作確認用のエージェントを
//! 本物のサンドボックスで起動して、依頼から承認・監査ログまでを通しで確かめる。
//!
//! サンドボックスの起動役は別のパッケージ（otas-adapter-linux）のバイナリなので、
//! `cargo test --workspace` で動かす（ワークスペース全体のバイナリがビルドされる）。

#![cfg(target_os = "linux")]

use otas_adapter_linux::LinuxSandbox;
use otas_audit::{FileAuditLog, verify_chain};
use otas_ipc::{ConsoleClient, Remote, connect_console};
use otas_types::{Outcome, TaskId};
use otasd::{Config, Daemon};
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::task::LocalSet;

fn launcher() -> PathBuf {
    // tests の実行ファイルは target/<profile>/deps/ にあり、バイナリは target/<profile>/ にある
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("otas-sandbox-exec");
    assert!(
        path.is_file(),
        "{} がありません。cargo test --workspace で実行してください",
        path.display()
    );
    path
}

fn agent() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_otas-demo-agent"))
}

fn config(name: &str) -> Config {
    let state_dir = std::env::temp_dir().join(format!("otasd-{name}-{}", TaskId::new()));
    Config::from_toml(&format!(
        "state_dir = {state:?}\nlauncher = {launcher:?}\n[demo]\ntools = true\n",
        state = state_dir.display().to_string(),
        launcher = launcher().display().to_string(),
    ))
    .unwrap()
}

fn run_local<F: Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, f);
}

/// デーモンを同じプロセスで動かし、人間向けの窓口に接続する。
async fn start_daemon(config: &Config) -> (Rc<Daemon>, ConsoleClient) {
    let sandbox = LinuxSandbox::detect(&config.launcher).unwrap();
    let daemon = Rc::new(Daemon::new(config.clone(), Arc::new(sandbox)).unwrap());
    let listener = daemon.bind().unwrap();
    let serving = daemon.clone();
    tokio::task::spawn_local(async move {
        let _ = serving.serve(listener).await;
    });
    let stream = UnixStream::connect(config.console_socket()).await.unwrap();
    (daemon, connect_console(stream))
}

/// タスクが終わるまで待ち、状態を返す。
async fn wait_for_exit(console: &ConsoleClient, task: TaskId) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = console
            .tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|(id, _)| *id == task)
            .map(|(_, s)| s)
            .expect("依頼したタスクが一覧にない");
        if status["state"] != "running" {
            return status;
        }
        assert!(Instant::now() < deadline, "タスクが終わらない: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn an_agent_in_the_sandbox_works_only_through_its_session() {
    run_local(async {
        let config = config("e2e");
        let (daemon, console) = start_daemon(&config).await;

        let steps = json!([
            {"tool": "demo.read_private"},
            {"tool": "demo.fetch_untrusted", "args": {"url": "https://example.com"}},
            {"tool": "demo.send", "args": {"to": "someone@example.com"}},
            {"read_file": "/etc/passwd"},
            {"tool": "mail.delete_all"}
        ]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [{"kind": "tool", "pattern": "demo.*"}]
        });
        let task = console.submit(&request.to_string()).await.unwrap();

        let status = wait_for_exit(&console, task).await;
        assert_eq!(status["state"], "exited");
        let log_dir = daemon.supervisor().log_dir(task);
        let stderr = std::fs::read_to_string(log_dir.join("stderr.log")).unwrap();
        assert_eq!(status["code"], 0, "エージェントが失敗した: {stderr}");

        let results = read_lines(&log_dir.join("stdout.log"));
        let kinds: Vec<&str> = results
            .iter()
            .map(|r| r["result"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "done",    // 私的データの読み取り
                "done",    // Web の取得（この時点では条件は 2 つ）
                "pending", // 3 つの条件がそろったので、送信は保留
                "error",   // ブローカーを通さないファイルの読み取りは、サンドボックスが止める
                "error",   // 登録されていないツール
            ],
            "{results:#?}"
        );
        assert!(
            results[3]["error"]
                .as_str()
                .unwrap()
                .contains("Permission denied"),
            "{}",
            results[3]
        );

        // 人間が承認すると、ブローカーが送信を実行する
        let approver = console.approver();
        let pending = approver.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task, task);
        let approved = approver.approve(pending[0].id).await.unwrap();
        assert!(matches!(approved, Remote::Done(_)), "{approved:?}");

        // 監査ログはハッシュチェーンとしてつながっていて、承認して実行した記録で終わる
        let records = FileAuditLog::read_all(config.audit_log()).unwrap();
        verify_chain(&records).unwrap();
        assert!(records.iter().all(|r| r.entry.task == task));
        let last = &records.last().unwrap().entry;
        assert!(last.approved_by_human);
        assert_eq!(last.outcome, Outcome::Executed);
    });
}

#[test]
fn a_request_that_cannot_start_is_reported() {
    run_local(async {
        let config = config("bad");
        let (_daemon, console) = start_daemon(&config).await;

        let relative = console
            .submit(&json!({"program": "otas-demo-agent"}).to_string())
            .await;
        assert!(relative.is_err());

        let unknown_field = console
            .submit(&json!({"program": agent(), "fs": []}).to_string())
            .await;
        assert!(unknown_field.is_err());
    });
}

/// 終わったら必ずデーモンを止める。
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn otas(socket: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_otas"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

#[test]
fn the_daemon_and_the_cli_work_as_separate_programs() {
    let config = config("bin");
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let config_path = config.state_dir.join("otasd.toml");
    std::fs::write(
        &config_path,
        format!(
            "state_dir = {:?}\nlauncher = {:?}\n[demo]\ntools = true\n",
            config.state_dir.display().to_string(),
            config.launcher.display().to_string()
        ),
    )
    .unwrap();

    let _daemon = Running(
        Command::new(env!("CARGO_BIN_EXE_otasd"))
            .arg("--config")
            .arg(&config_path)
            .spawn()
            .unwrap(),
    );
    let socket = config.console_socket();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "otasd が待ち受けを始めない");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 依頼：私的データを読み、Web を取得し、送信しようとする
    let steps = json!([
        {"tool": "demo.read_private"},
        {"tool": "demo.fetch_untrusted"},
        {"tool": "demo.send"}
    ]);
    let request_path = config.state_dir.join("request.json");
    std::fs::write(
        &request_path,
        json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [{"kind": "tool", "pattern": "demo.*"}]
        })
        .to_string(),
    )
    .unwrap();
    let (ok, out) = otas(&socket, &["submit", request_path.to_str().unwrap()]);
    assert!(ok, "{out}");
    let task = out.trim().to_string();

    // 終わるまで待つ
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (ok, out) = otas(&socket, &["tasks"]);
        assert!(ok, "{out}");
        if out.contains(&task) && out.contains("\"exited\"") {
            break;
        }
        assert!(Instant::now() < deadline, "タスクが終わらない: {out}");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 承認待ちを確かめて承認する
    let (ok, out) = otas(&socket, &["pending"]);
    assert!(ok, "{out}");
    let line = out
        .lines()
        .find(|l| l.contains(&task))
        .expect("承認待ちがない");
    let approval = line.split('\t').next().unwrap().to_string();
    assert!(line.contains("demo.send"), "{line}");

    let (ok, out) = otas(&socket, &["approve", &approval]);
    assert!(ok, "{out}");
    assert!(out.contains("実行しました"), "{out}");

    let (ok, out) = otas(&socket, &["pending"]);
    assert!(ok && !out.contains(&task), "{out}");
}
