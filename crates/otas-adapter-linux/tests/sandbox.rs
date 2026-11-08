//! Linux サンドボックスの結合テスト。実際に `/bin/sh` などをサンドボックスで動かして確かめる。

#![cfg(target_os = "linux")]

use otas_adapter_linux::{LAUNCHER_FAILURE_EXIT_CODE, LinuxSandbox};
use otas_ports::{FsAccess, ResourceLimits, Sandbox, SandboxError, SandboxSpec};
use otas_types::{Level, TaskId};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn sandbox() -> LinuxSandbox {
    LinuxSandbox::detect(env!("CARGO_BIN_EXE_otas-sandbox-exec"))
        .expect("この環境ではサンドボックスを使えません")
}

/// テストごとに別のディレクトリを作る。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("otas-sbx-{name}-{}", TaskId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Run {
    code: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// サンドボックスで動かし、終了状態と出力を集める。
fn run(sb: &LinuxSandbox, mut spec: SandboxSpec, stdin_data: Option<(UnixStream, &[u8])>) -> Run {
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let (mut err_r, err_w) = std::io::pipe().unwrap();
    spec.stdout = Stdio::from(out_w);
    spec.stderr = Stdio::from(err_w);
    let mut process = sb.spawn(spec).unwrap();

    if let Some((mut channel, data)) = stdin_data {
        channel.write_all(data).unwrap();
        channel.shutdown(std::net::Shutdown::Write).unwrap();
    }

    let info = process.wait().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    out_r.read_to_string(&mut stdout).unwrap();
    err_r.read_to_string(&mut stderr).unwrap();
    Run {
        code: info.code,
        signal: info.signal,
        stdout,
        stderr,
    }
}

fn sh(script: &str) -> SandboxSpec {
    SandboxSpec::new("/bin/sh").arg("-c").arg(script)
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

#[test]
fn reads_granted_data_and_writes_only_to_the_workspace() {
    let sb = sandbox();
    let data = temp_dir("data");
    let work = temp_dir("work");
    std::fs::write(data.join("in.txt"), "こんにちは").unwrap();

    let spec = sh(&format!(
        "cat {d}/in.txt > {w}/out.txt",
        d = path(&data),
        w = path(&work)
    ))
    .allow(&data, FsAccess::ReadOnly)
    .allow(&work, FsAccess::ReadWrite);
    let r = run(&sb, spec, None);
    assert!(r.ok(), "失敗: {}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(work.join("out.txt")).unwrap(),
        "こんにちは"
    );

    // 読み取り専用の場所には書けない
    let spec = sh(&format!("echo x > {d}/new.txt", d = path(&data)))
        .allow(&data, FsAccess::ReadOnly)
        .allow(&work, FsAccess::ReadWrite);
    let r = run(&sb, spec, None);
    assert!(!r.ok());
    assert!(r.stderr.contains("Permission denied"), "{}", r.stderr);
    assert!(!data.join("new.txt").exists());
}

#[test]
fn paths_outside_the_rules_cannot_be_read() {
    let sb = sandbox();
    let granted = temp_dir("granted");
    let other = temp_dir("other");
    std::fs::write(other.join("secret.txt"), "見えてはいけない").unwrap();

    // 許可していない兄弟ディレクトリ
    let spec =
        sh(&format!("cat {o}/secret.txt", o = path(&other))).allow(&granted, FsAccess::ReadOnly);
    let r = run(&sb, spec, None);
    assert!(!r.ok());
    assert!(r.stdout.is_empty());
    assert!(r.stderr.contains("Permission denied"), "{}", r.stderr);

    // システムの設定ファイル
    let r = run(&sb, sh("cat /etc/passwd"), None);
    assert!(!r.ok());
    assert!(r.stdout.is_empty());
}

#[test]
fn network_is_denied_by_the_sandbox_not_by_the_peer() {
    let sb = sandbox();
    let spec = SandboxSpec::new("/bin/bash")
        .arg("-c")
        .arg("exec 3<>/dev/tcp/127.0.0.1/80");
    let r = run(&sb, spec, None);
    assert!(!r.ok());
    // 接続先に断られたのではなく、ソケット自体を作れないこと
    assert!(!r.stderr.contains("Connection refused"), "{}", r.stderr);
    assert!(
        r.stderr.contains("not permitted") || r.stderr.contains("Permission denied"),
        "{}",
        r.stderr
    );
}

#[test]
fn namespaces_cannot_be_created() {
    let sb = sandbox();
    let r = run(
        &sb,
        SandboxSpec::new("/usr/bin/unshare")
            .arg("--user")
            .arg("/bin/true"),
        None,
    );
    assert!(!r.ok());
    assert!(r.stderr.contains("not permitted"), "{}", r.stderr);
}

#[test]
fn the_parent_environment_is_not_inherited() {
    let sb = sandbox();
    // CARGO_HOME はテストを動かしているコンテナでは設定されている
    assert!(std::env::var("CARGO_HOME").is_ok());
    let spec = sh(r#"printf '[%s][%s]' "$CARGO_HOME" "$OTAS_VISIBLE""#).env("OTAS_VISIBLE", "yes");
    let r = run(&sb, spec, None);
    assert!(r.ok(), "{}", r.stderr);
    assert_eq!(r.stdout, "[][yes]");
}

#[test]
fn resource_limits_are_applied() {
    let sb = sandbox();
    let work = temp_dir("limits");
    let mut spec = sh(&format!(
        "ulimit -c; head -c 4096 /dev/zero > {w}/big",
        w = path(&work)
    ))
    .allow(&work, FsAccess::ReadWrite);
    spec.limits = ResourceLimits {
        file_size_bytes: Some(1024),
        ..ResourceLimits::default()
    };
    let r = run(&sb, spec, None);
    // コアダンプは常に無効
    assert_eq!(r.stdout.trim(), "0");
    // 上限を超えた書き込みは止められる
    assert!(!r.ok());
    let size = std::fs::metadata(work.join("big"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(size <= 1024, "{size} バイト書けてしまった");
}

#[test]
fn stdin_can_carry_a_channel_to_the_broker() {
    let sb = sandbox();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut spec = SandboxSpec::new("/bin/cat");
    spec.stdin = Stdio::from(OwnedFd::from(theirs));
    let r = run(&sb, spec, Some((ours, "ping".as_bytes())));
    assert!(r.ok(), "{}", r.stderr);
    assert_eq!(r.stdout, "ping");
}

#[test]
fn missing_required_paths_stop_the_launch() {
    let sb = sandbox();
    let spec = sh("true").allow("/nonexistent/otas", FsAccess::ReadOnly);
    let r = run(&sb, spec, None);
    assert_eq!(r.code, Some(LAUNCHER_FAILURE_EXIT_CODE));
    assert!(r.signal.is_none());
    assert!(r.stderr.contains("/nonexistent/otas"), "{}", r.stderr);
}

#[test]
fn invalid_specs_are_rejected_before_launch() {
    let sb = sandbox();
    let relative = sb.spawn(SandboxSpec::new("sh"));
    assert!(matches!(relative, Err(SandboxError::InvalidSpec(_))));

    let dotdot = sb.spawn(sh("true").allow("/tmp/../etc", FsAccess::ReadOnly));
    assert!(matches!(dotdot, Err(SandboxError::InvalidSpec(_))));
}

#[test]
fn guarantees_are_reported_honestly() {
    let g = sandbox().guarantees();
    assert_eq!(g.fs_isolation, Level::Strong);
    assert_eq!(g.net_isolation, Level::Strong);
    // 拒否リスト方式なので Strong とは名乗らない
    assert_eq!(g.syscall_filter, Level::Weak);
    // 作業領域（スナップショット）はまだない
    assert_eq!(g.snapshot, Level::Absent);
}
