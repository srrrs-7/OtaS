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

/// 作業領域：変更はコミットするまで正本に反映されず、ラベルはファイルを通じて次のタスクに伝わる。
#[test]
fn edits_wait_in_the_workspace_and_labels_follow_the_files() {
    run_local(async {
        let state_dir = std::env::temp_dir().join(format!("otasd-ws-{}", TaskId::new()));
        // 正本の初期データ（デーモンが最初に開いたときに版 1 として取り込まれる）
        let live = state_dir.join("store/live");
        std::fs::create_dir_all(live.join("projects/a")).unwrap();
        std::fs::create_dir_all(live.join("mail")).unwrap();
        std::fs::write(live.join("projects/a/notes.md"), "v1").unwrap();
        std::fs::write(live.join("mail/1.eml"), "私的なメール").unwrap();

        let config = Config::from_toml(&format!(
            "state_dir = {:?}\nlauncher = {:?}\n\
             [[data.labels]]\npath = \"/data/mail\"\nlabels = [\"private\"]\n\
             [demo]\ntools = true\n",
            state_dir.display().to_string(),
            launcher().display().to_string(),
        ))
        .unwrap();
        let (daemon, console) = start_daemon(&config).await;
        assert_eq!(daemon.data_dir(), live);

        // タスク A：メールを読み、メモを書き換え、Web を取得して、送信しようとする
        let steps = json!([
            {"read_file": "$OTAS_DATA/mail/1.eml"},
            {"write_file": "$OTAS_DATA/projects/a/notes.md", "content": "v2"},
            {"write_file": "$OTAS_DATA/mail/1.eml", "content": "改ざん"},
            {"tool": "demo.fetch_untrusted"},
            {"tool": "demo.send"}
        ]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [
                {"kind": "write", "prefix": "/data/projects/a"},
                {"kind": "read", "prefix": "/data/mail"},
                {"kind": "tool", "pattern": "demo.*"}
            ]
        });
        let a = console.submit(&request.to_string()).await.unwrap();
        let status = wait_for_exit(&console, a).await;
        assert_eq!(status["state"], "awaiting_review", "{status}");
        assert_eq!(status["changes"], 1);

        let results = read_lines(&daemon.supervisor().log_dir(a).join("stdout.log"));
        let kinds: Vec<&str> = results
            .iter()
            .map(|r| r["result"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "done",    // 見せた範囲のメールは読める
                "done",    // 書き込める範囲には書ける（作業領域の中だけ）
                "error",   // 読み取りだけの範囲には書けない
                "done",    // Web の取得（私的データ＋外部への作用で条件は 2 つ）
                "pending", // メールを見せた時点で私的ラベルが付いているので、3 つそろって保留
            ],
            "{results:#?}"
        );

        // コミットするまで正本は変わらない
        assert_eq!(
            std::fs::read_to_string(live.join("projects/a/notes.md")).unwrap(),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(live.join("mail/1.eml")).unwrap(),
            "私的なメール"
        );

        let diff = console.review(a).await.unwrap();
        assert_eq!(
            diff,
            json!([{"path": "/data/projects/a/notes.md", "kind": "modified"}])
        );

        let committed = console.commit(a).await.unwrap();
        assert_eq!(committed["result"], "applied", "{committed}");
        assert_eq!(committed["version"], 2);
        assert_eq!(
            std::fs::read_to_string(live.join("projects/a/notes.md")).unwrap(),
            "v2"
        );

        // タスク B：projects/a だけを見せる。メールは見せていないが、A が書いたメモには
        // A のラベル（私的・信頼できない）が付いているので、送信はすぐに保留される
        let steps = json!([{"tool": "demo.send"}]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [
                {"kind": "write", "prefix": "/data/projects/a"},
                {"kind": "tool", "pattern": "demo.*"}
            ]
        });
        let b = console.submit(&request.to_string()).await.unwrap();
        let status = wait_for_exit(&console, b).await;
        assert_eq!(status["state"], "exited", "{status}");
        let results = read_lines(&daemon.supervisor().log_dir(b).join("stdout.log"));
        assert_eq!(results[0]["result"], "pending", "{results:#?}");

        // 正本の版と巻き戻し
        let versions = console.versions().await.unwrap();
        assert_eq!(versions, json!({"current": 2, "versions": [1, 2]}));
        let rolled = console.rollback(1).await.unwrap();
        assert_eq!(rolled["version"], 3);
        assert_eq!(
            std::fs::read_to_string(live.join("projects/a/notes.md")).unwrap(),
            "v1"
        );

        // コミットの書き込みも、ブローカーの判定を通って監査ログに残る
        let records = FileAuditLog::read_all(config.audit_log()).unwrap();
        verify_chain(&records).unwrap();
        assert!(records.iter().any(|r| r.entry.task == a
            && matches!(&r.entry.action, otas_types::Action::WriteData { path } if path == "/data/projects/a/notes.md")));
    });
}

#[test]
fn discarded_changes_never_reach_the_data() {
    run_local(async {
        let config = config("discard");
        let (daemon, console) = start_daemon(&config).await;
        let steps = json!([{"write_file": "$OTAS_DATA/projects/x/draft.md", "content": "下書き"}]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [{"kind": "write", "prefix": "/data/projects/x"}]
        });
        let task = console.submit(&request.to_string()).await.unwrap();
        assert_eq!(
            wait_for_exit(&console, task).await["state"],
            "awaiting_review"
        );

        console.discard(task).await.unwrap();
        assert!(!daemon.data_dir().join("projects/x/draft.md").exists());
        let tasks = console.tasks().await.unwrap();
        let status = &tasks.iter().find(|(id, _)| *id == task).unwrap().1;
        assert_eq!(status["state"], "discarded");
        // 捨てた後はコミットできない
        assert!(console.commit(task).await.is_err());
    });
}

/// ループバックで動く偽の HTTP サーバ。どの要求にも同じ本文を返し、受け取った要求の先頭部分を記録する。
async fn fake_http(body: String) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 65536];
            let mut len = 0;
            // 先頭と本文を読み切る（content-length の分だけ）
            loop {
                let n = socket.read(&mut buf[len..]).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                len += n;
                let text = String::from_utf8_lossy(&buf[..len]).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    let want = text[..pos]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if len >= pos + 4 + want {
                        break;
                    }
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..len]).to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// モデル：エージェントは API キーに触れられず、クラウドのモデルへの送信は Rule of Two で保留される。
#[test]
fn agents_use_models_only_through_the_broker() {
    run_local(async {
        let (local_url, _) = fake_http(
            json!({"choices": [{"message": {"content": "ローカルの応答"}, "finish_reason": "stop"}]})
                .to_string(),
        )
        .await;
        let (claude_url, claude_seen) = fake_http(
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "Claude の応答"}]})
                .to_string(),
        )
        .await;

        let state_dir = std::env::temp_dir().join(format!("otasd-models-{}", TaskId::new()));
        std::fs::create_dir_all(&state_dir).unwrap();
        let key_file = state_dir.join("anthropic.key");
        std::fs::write(&key_file, "sk-ant-e2e-secret\n").unwrap();

        let config = Config::from_toml(&format!(
            "state_dir = {state:?}\nlauncher = {launcher:?}\n\
             [demo]\ntools = true\n\
             [[models]]\nid = \"local\"\nprovider = \"openai_compatible\"\nmodel = \"llama3\"\n\
             base_url = \"{local_url}/v1\"\nlocal = true\n\
             [[models]]\nid = \"claude\"\nprovider = \"anthropic\"\nbase_url = \"{claude_url}\"\n\
             api_key_file = {key:?}\n",
            state = state_dir.display().to_string(),
            launcher = launcher().display().to_string(),
            key = key_file.display().to_string(),
        ))
        .unwrap();
        let (daemon, console) = start_daemon(&config).await;

        let steps = json!([
            {"read_file": key_file},
            {"tool": "demo.read_private"},
            {"tool": "demo.fetch_untrusted"},
            {"infer": "local", "prompt": "要約して"},
            {"infer": "claude", "prompt": "要約して"}
        ]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [
                {"kind": "tool", "pattern": "demo.*"},
                {"kind": "model", "pattern": "*"}
            ]
        });
        let task = console.submit(&request.to_string()).await.unwrap();
        let status = wait_for_exit(&console, task).await;
        assert_eq!(status["state"], "exited", "{status}");

        let results = read_lines(&daemon.supervisor().log_dir(task).join("stdout.log"));
        let kinds: Vec<&str> = results
            .iter()
            .map(|r| r["result"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "error",   // API キーのファイルはサンドボックスから読めない
                "done",    // 私的データ
                "done",    // 信頼できない入力
                "done",    // ローカルのモデルは外に出ないので通る
                "pending", // クラウドのモデルへの送信は 3 条件がそろうので保留
            ],
            "{results:#?}"
        );
        assert_eq!(results[3]["text"], "ローカルの応答");
        // 保留中は、クラウドに何も送っていない
        assert!(claude_seen.lock().unwrap().is_empty());

        // 承認すると、デーモンが API キーを付けて送る
        let approver = console.approver();
        let pending = approver.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let done = approver.approve(pending[0].id).await.unwrap();
        assert_eq!(done, Remote::Done(json!("Claude の応答")));
        let sent = claude_seen.lock().unwrap()[0].clone();
        assert!(
            sent.to_ascii_lowercase()
                .contains("x-api-key: sk-ant-e2e-secret"),
            "{sent}"
        );
    });
}

#[test]
fn a_missing_api_key_stops_the_daemon_from_starting() {
    let state_dir = std::env::temp_dir().join(format!("otasd-nokey-{}", TaskId::new()));
    let config = Config::from_toml(&format!(
        "state_dir = {state:?}\nlauncher = {launcher:?}\n\
         [[models]]\nid = \"claude\"\nprovider = \"anthropic\"\napi_key_env = \"OTAS_TEST_KEY_THAT_DOES_NOT_EXIST\"\n",
        state = state_dir.display().to_string(),
        launcher = launcher().display().to_string(),
    ))
    .unwrap();
    let sandbox = LinuxSandbox::detect(&config.launcher).unwrap();
    let result = Daemon::new(config, Arc::new(sandbox));
    assert!(matches!(result, Err(otasd::DaemonError::Model(_))));
}

/// 再起動：承認待ち、レビュー待ちの作業領域、タスクのラベルは、デーモンを再起動しても残る。
#[test]
fn approvals_and_reviews_survive_a_daemon_restart() {
    let state_dir = std::env::temp_dir().join(format!("otasd-restart-{}", TaskId::new()));
    let config = Config::from_toml(&format!(
        "state_dir = {state:?}\nlauncher = {launcher:?}\n\
         [[data.labels]]\npath = \"/data/mail\"\nlabels = [\"private\"]\n\
         [demo]\ntools = true\n",
        state = state_dir.display().to_string(),
        launcher = launcher().display().to_string(),
    ))
    .unwrap();

    // 1回目の起動：タスクが送信を保留し、作業領域に変更を残して終わる。もう1つのタスクは動いたまま
    let (task, sleeper) = {
        let config = config.clone();
        let mut ids = None;
        run_local(async {
            let (_daemon, console) = start_daemon(&config).await;
            let steps = json!([
                {"write_file": "$OTAS_DATA/projects/a/report.md", "content": "報告"},
                {"tool": "demo.fetch_untrusted"},
                {"tool": "demo.send"}
            ]);
            let request = json!({
                "program": agent(),
                "args": [steps.to_string()],
                "grants": [
                    {"kind": "read", "prefix": "/data/mail"},
                    {"kind": "write", "prefix": "/data/projects/a"},
                    {"kind": "tool", "pattern": "demo.*"}
                ]
            });
            let task = console.submit(&request.to_string()).await.unwrap();
            assert_eq!(
                wait_for_exit(&console, task).await["state"],
                "awaiting_review"
            );
            assert_eq!(console.approver().pending().await.unwrap().len(), 1);

            let sleeper = console
                .submit(&json!({"program": "/bin/sleep", "args": ["3"]}).to_string())
                .await
                .unwrap();
            ids = Some((task, sleeper));
        });
        ids.unwrap()
    };

    // 2回目の起動：同じ設定（同じ状態ディレクトリ）で起動し直す
    run_local(async {
        let (daemon, console) = start_daemon(&config).await;
        let tasks = console.tasks().await.unwrap();
        let status = |id: TaskId| tasks.iter().find(|(t, _)| *t == id).unwrap().1.clone();
        assert_eq!(status(task)["state"], "awaiting_review");
        // 動いていたタスクは、変更がないので中断として扱われる
        assert_eq!(status(sleeper)["state"], "interrupted");

        // 承認待ちが残っていて、承認すれば実行される
        let approver = console.approver();
        let pending = approver.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task, task);
        assert!(matches!(
            approver.approve(pending[0].id).await.unwrap(),
            Remote::Done(_)
        ));

        // レビュー待ちの変更をコミットできる。書いたファイルには、再起動前に積み上がったラベルが付く
        let committed = console.commit(task).await.unwrap();
        assert_eq!(committed["result"], "applied", "{committed}");
        assert_eq!(
            std::fs::read_to_string(daemon.data_dir().join("projects/a/report.md")).unwrap(),
            "報告"
        );
        let labels: Value = serde_json::from_slice(
            &std::fs::read(config.store_dir().join("meta/labels.json")).unwrap(),
        )
        .unwrap();
        let report = &labels["projects/a/report.md"];
        assert!(
            report.as_array().unwrap().contains(&json!("private"))
                && report.as_array().unwrap().contains(&json!("untrusted")),
            "{labels}"
        );

        // 監査ログは再起動をまたいでつながっている
        let records = FileAuditLog::read_all(config.audit_log()).unwrap();
        verify_chain(&records).unwrap();
    });
}

/// 名前空間の方式：エージェントには作業領域が `/data` として見え、それ以外は存在しない。
/// 書き込みは OverlayFS の上の層に入り、コミットするまで正本は変わらない。
/// 特権を足したテスト用のコンテナ（OTAS_TEST_PRIVILEGED=1）でだけ動かす。
/// OTAS_TEST_BTRFS_DIR があれば、版を Btrfs の読み取り専用スナップショットにする。
#[test]
fn namespace_mode_shows_only_data_and_commits_through_overlay() {
    if std::env::var("OTAS_TEST_PRIVILEGED").as_deref() != Ok("1") {
        eprintln!("特権を足したテスト用のコンテナでだけ動かします（OTAS_TEST_PRIVILEGED=1）");
        return;
    }
    let btrfs = std::env::var("OTAS_TEST_BTRFS_DIR").ok();
    let base = btrfs
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let state_dir = base.join(format!("otasd-ns-{}", TaskId::new()));
    let live = state_dir.join("store/live");
    if btrfs.is_some() {
        // Btrfs では、正本をサブボリュームとして先に作る（サブボリュームをまたぐ rename はできない）
        std::fs::create_dir_all(state_dir.join("store")).unwrap();
        let ok = Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&live)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    std::fs::create_dir_all(live.join("projects/a")).unwrap();
    std::fs::create_dir_all(live.join("mail")).unwrap();
    std::fs::write(live.join("projects/a/notes.md"), "v1").unwrap();
    std::fs::write(live.join("mail/1.eml"), "私的なメール").unwrap();

    let versions = if btrfs.is_some() { "btrfs" } else { "hardlink" };
    let config = Config::from_toml(&format!(
        "state_dir = {state:?}\nlauncher = {launcher:?}\n\
         [sandbox]\nmode = \"namespace\"\nuid = 10001\ngid = 10001\n\
         [data]\nversions = \"{versions}\"\n\
         [[data.labels]]\npath = \"/data/mail\"\nlabels = [\"private\"]\n\
         [demo]\ntools = true\n",
        state = state_dir.display().to_string(),
        launcher = launcher().display().to_string(),
    ))
    .unwrap();

    run_local(async {
        let sandbox = otasd::sandbox_from_config(&config).unwrap();
        let daemon = Rc::new(Daemon::new(config.clone(), sandbox).unwrap());
        let listener = daemon.bind().unwrap();
        let serving = daemon.clone();
        tokio::task::spawn_local(async move {
            let _ = serving.serve(listener).await;
        });
        let console = connect_console(UnixStream::connect(config.console_socket()).await.unwrap());

        let steps = json!([
            {"read_file": "/data/mail/1.eml"},
            {"write_file": "/data/projects/a/notes.md", "content": "v2"},
            {"write_file": "/data/mail/1.eml", "content": "改ざん"},
            {"read_file": "/etc/passwd"},
            {"read_file": "$OTAS_DATA/projects/a/notes.md"},
            {"tool": "demo.fetch_untrusted"},
            {"tool": "demo.send"}
        ]);
        let request = json!({
            "program": agent(),
            "args": [steps.to_string()],
            "grants": [
                {"kind": "read", "prefix": "/data/mail"},
                {"kind": "write", "prefix": "/data/projects/a"},
                {"kind": "tool", "pattern": "demo.*"}
            ]
        });
        let task = console.submit(&request.to_string()).await.unwrap();
        let status = wait_for_exit(&console, task).await;
        let stderr =
            std::fs::read_to_string(daemon.supervisor().log_dir(task).join("stderr.log")).unwrap();
        assert_eq!(status["state"], "awaiting_review", "{status} {stderr}");

        let results = read_lines(&daemon.supervisor().log_dir(task).join("stdout.log"));
        let kinds: Vec<&str> = results
            .iter()
            .map(|r| r["result"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec!["done", "done", "error", "error", "done", "done", "pending"],
            "{results:#?}"
        );
        assert!(
            results[2]["error"].as_str().unwrap().contains("Read-only"),
            "{}",
            results[2]
        );
        // /etc/passwd は読めないのではなく、存在しない
        assert!(
            results[3]["error"]
                .as_str()
                .unwrap()
                .contains("No such file"),
            "{}",
            results[3]
        );

        // 正本はまだ変わらない。変更は上の層にある
        assert_eq!(
            std::fs::read_to_string(live.join("projects/a/notes.md")).unwrap(),
            "v1"
        );
        assert_eq!(
            console.review(task).await.unwrap(),
            json!([{"path": "/data/projects/a/notes.md", "kind": "modified"}])
        );
        let committed = console.commit(task).await.unwrap();
        assert_eq!(committed["result"], "applied", "{committed}");
        assert_eq!(
            std::fs::read_to_string(live.join("projects/a/notes.md")).unwrap(),
            "v2"
        );
        // 正本のファイルはタスクのユーザーのもの
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(live.join("projects/a/notes.md"))
                .unwrap()
                .uid(),
            10001
        );
        if btrfs.is_some() {
            // 版は読み取り専用のスナップショット
            let err = std::fs::write(state_dir.join("store/versions/2/projects/a/notes.md"), "x")
                .unwrap_err();
            assert_eq!(err.raw_os_error(), Some(30));
        }
    });
}

/// 決まった応答（SSE）を順に返す偽の Claude API。受け取った要求の本文（JSON）を記録する。
async fn scripted_claude(
    responses: Vec<String>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for body in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    let want = text[..pos]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + want {
                        let request: Value =
                            serde_json::from_slice(&buf[pos + 4..pos + 4 + want]).unwrap();
                        log.lock().unwrap().push(request);
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// ツールを1つ呼ぶ応答（SSE）。思考のブロック（署名つき）を先頭に付ける。
fn tool_use_stream(signature: &str, id: &str, name: &str, input: Value) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": format!("msg_{id}")}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "signature_delta", "signature": signature}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
               "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 1,
               "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
        .collect()
}

/// エージェントのループ：モデルがツールを選び、ブローカーとサンドボックスを通って実行される。
/// 私的データと信頼できない入力の両方に触れた後は、クラウドのモデルへの送信が承認待ちになって止まる。
#[test]
fn the_agent_loop_runs_tools_and_stops_when_the_model_call_needs_approval() {
    run_local(async {
        let (claude_url, requests) = scripted_claude(vec![
            tool_use_stream("sig-1", "toolu_1", "demo__read_private", json!({})),
            tool_use_stream(
                "sig-2",
                "toolu_2",
                "fs__write",
                json!({"path": "projects/a/summary.md", "content": "要約"}),
            ),
            tool_use_stream(
                "sig-3",
                "toolu_3",
                "demo__fetch_untrusted",
                json!({"url": "https://example.com"}),
            ),
        ])
        .await;

        let state_dir = std::env::temp_dir().join(format!("otasd-agent-{}", TaskId::new()));
        std::fs::create_dir_all(&state_dir).unwrap();
        let key_file = state_dir.join("anthropic.key");
        std::fs::write(&key_file, "sk-ant-agent-test").unwrap();
        let config = Config::from_toml(&format!(
            "state_dir = {state:?}\nlauncher = {launcher:?}\n[demo]\ntools = true\n\
             [[models]]\nid = \"claude\"\nprovider = \"anthropic\"\nbase_url = \"{claude_url}\"\n\
             api_key_file = {key:?}\n",
            state = state_dir.display().to_string(),
            launcher = launcher().display().to_string(),
            key = key_file.display().to_string(),
        ))
        .unwrap();
        let (daemon, console) = start_daemon(&config).await;

        let request = json!({
            "program": env!("CARGO_BIN_EXE_otas-agent"),
            "args": ["--model", "claude", "私的データを要約して projects/a/summary.md に保存し、関連する Web ページも確認して"],
            "grants": [
                {"kind": "write", "prefix": "/data/projects/a"},
                {"kind": "tool", "pattern": "demo.*"},
                {"kind": "model", "pattern": "claude"}
            ]
        });
        let task = console.submit(&request.to_string()).await.unwrap();
        let status = wait_for_exit(&console, task).await;
        let log_dir = daemon.supervisor().log_dir(task);
        let stderr = std::fs::read_to_string(log_dir.join("stderr.log")).unwrap();
        // モデルへの送信が承認待ちで止まり（終了コード 3）、作業領域の変更はレビュー待ち
        assert_eq!(status["state"], "awaiting_review", "{status} {stderr}");
        assert_eq!(status["code"], 3, "{stderr}");

        let lines = read_lines(&log_dir.join("stdout.log"));
        let tools: Vec<(&str, &str)> = lines
            .iter()
            .filter(|l| l["kind"] == "tool")
            .map(|l| (l["name"].as_str().unwrap(), l["result"].as_str().unwrap()))
            .collect();
        assert_eq!(
            tools,
            vec![
                ("demo.read_private", "done"),
                ("fs__write", "done"),
                ("demo.fetch_untrusted", "done"),
            ],
            "{lines:#?}"
        );
        assert_eq!(lines.last().unwrap()["result"], "pending", "{lines:#?}");

        // クラウドに届いたのは 3 回だけ（4 回目は保留中）
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        let first = &requests[0];
        assert_eq!(first["stream"], true);
        let tool_names: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(
            tool_names.contains(&"fs__write") && tool_names.contains(&"demo__send"),
            "{tool_names:?}"
        );
        // 2 回目：前の応答（思考のブロックの署名を含む）をそのまま送り返し、ツールの結果を付ける
        let second = &requests[1]["messages"];
        assert_eq!(second[1]["role"], "assistant");
        assert_eq!(second[1]["content"][0]["type"], "thinking");
        assert_eq!(second[1]["content"][0]["signature"], "sig-1");
        assert_eq!(second[2]["content"][0]["type"], "tool_result");
        assert_eq!(second[2]["content"][0]["tool_use_id"], "toolu_1");

        // 承認待ちの一覧には、クラウドのモデルへの送信がある
        let pending = console.approver().pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].action,
            otas_types::Action::Infer {
                model: "claude".into()
            }
        );
        // エージェントが書いたファイルは、レビューを待っている
        assert_eq!(
            console.review(task).await.unwrap(),
            json!([{"path": "/data/projects/a/summary.md", "kind": "added"}])
        );
    });
}
