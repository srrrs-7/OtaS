//! 名前空間を使うサンドボックスのテスト。
//!
//! root と mount が必要なので、特権を足したテスト用のコンテナ（compose の `dev-privileged`）で
//! `OTAS_TEST_PRIVILEGED=1` のときだけ動かす。それ以外では、root でないと使えないことだけを確かめる。

#![cfg(target_os = "linux")]

use otas_adapter_linux::{LinuxSandbox, NamespaceConfig};
use otas_ports::{Mount, MountSource, Sandbox, SandboxError, SandboxSpec};
use otas_types::TaskId;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

const TASK_UID: u32 = 10001;

fn privileged() -> bool {
    std::env::var("OTAS_TEST_PRIVILEGED").as_deref() == Ok("1")
}

macro_rules! require_privileged {
    () => {
        if !privileged() {
            eprintln!("特権を足したテスト用のコンテナでだけ動かします（OTAS_TEST_PRIVILEGED=1）");
            return;
        }
    };
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("otas-ns-{name}-{}", TaskId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sandbox() -> LinuxSandbox {
    LinuxSandbox::detect_namespaced(
        env!("CARGO_BIN_EXE_otas-sandbox-exec"),
        NamespaceConfig {
            uid: TASK_UID,
            gid: TASK_UID,
            run_dir: temp_dir("run"),
        },
    )
    .expect("名前空間のサンドボックスを使えません")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run(sb: &LinuxSandbox, mut spec: SandboxSpec) -> Run {
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let (mut err_r, err_w) = std::io::pipe().unwrap();
    spec.stdout = Stdio::from(out_w);
    spec.stderr = Stdio::from(err_w);
    let mut process = sb.spawn(spec).unwrap();
    let info = process.wait().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    out_r.read_to_string(&mut stdout).unwrap();
    err_r.read_to_string(&mut stderr).unwrap();
    Run {
        ok: info.success(),
        stdout,
        stderr,
    }
}

fn sh(script: &str) -> SandboxSpec {
    SandboxSpec::new("/bin/sh").arg("-c").arg(script)
}

fn chown_tree(path: &Path) {
    std::os::unix::fs::chown(path, Some(TASK_UID), Some(TASK_UID)).unwrap();
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap() {
            chown_tree(&e.unwrap().path());
        }
    }
}

#[test]
fn only_root_can_use_namespaces() {
    if rustix::process::getuid().is_root() {
        return;
    }
    let r = LinuxSandbox::detect_namespaced(
        env!("CARGO_BIN_EXE_otas-sandbox-exec"),
        NamespaceConfig {
            uid: TASK_UID,
            gid: TASK_UID,
            run_dir: temp_dir("run"),
        },
    );
    assert!(matches!(r, Err(SandboxError::Unsupported(_))));
}

#[test]
fn paths_that_were_not_granted_do_not_exist() {
    require_privileged!();
    // Landlock は stat を制限しないので、`test -e` で「存在するか」を確かめられる
    // （`/` の一覧は Landlock が拒否するので ls は使えない）
    let script = "for p in /usr /tmp /dev/null /home /root /workspace /cache /etc/passwd; do \
                  if [ -e $p ]; then echo \"yes $p\"; else echo \"no $p\"; fi; done; cat /etc/passwd";
    let r = run(&sandbox(), sh(script));
    assert!(!r.ok);
    // /etc/passwd は「読めない」のではなく「存在しない」
    assert!(r.stderr.contains("No such file"), "{}", r.stderr);
    let expected = "yes /usr\nyes /tmp\nyes /dev/null\nno /home\nno /root\nno /workspace\nno /cache\nno /etc/passwd\n";
    assert_eq!(r.stdout, expected);
}

#[test]
fn tasks_run_as_an_unprivileged_user() {
    require_privileged!();
    let r = run(&sandbox(), sh("id -u; id -g"));
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{TASK_UID}\n{TASK_UID}\n"));
}

#[test]
fn the_root_is_read_only_and_tmp_is_private() {
    require_privileged!();
    let sb = sandbox();
    let r = run(&sb, sh("touch /newfile"));
    assert!(!r.ok);
    assert!(r.stderr.contains("Read-only file system"), "{}", r.stderr);

    let r = run(&sb, sh("echo hi > /tmp/x && cat /tmp/x"));
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(r.stdout, "hi\n");
}

#[test]
fn the_network_namespace_is_empty() {
    require_privileged!();
    let r = run(
        &sandbox(),
        SandboxSpec::new("/bin/bash")
            .arg("-c")
            .arg("exec 3<>/dev/tcp/127.0.0.1/80"),
    );
    assert!(!r.ok);
    assert!(!r.stderr.contains("Connection refused"), "{}", r.stderr);
}

#[test]
fn bind_mounts_appear_at_their_target() {
    require_privileged!();
    let docs = temp_dir("docs");
    std::fs::write(docs.join("a.txt"), "資料").unwrap();
    let out = temp_dir("out");
    chown_tree(&out);

    let mut spec =
        sh("cat /data/docs/a.txt; echo x > /data/docs/b.txt; echo y > /data/out/c.txt; echo done");
    spec.mounts = vec![
        Mount {
            target: "/data/docs".into(),
            source: MountSource::Path(docs.clone()),
            writable: false,
        },
        Mount {
            target: "/data/out".into(),
            source: MountSource::Path(out.clone()),
            writable: true,
        },
    ];
    let r = run(&sandbox(), spec);
    assert_eq!(r.stdout, "資料done\n");
    assert!(r.stderr.contains("Read-only file system"), "{}", r.stderr);
    assert!(!docs.join("b.txt").exists());
    let written = std::fs::metadata(out.join("c.txt")).unwrap();
    assert_eq!(written.uid(), TASK_UID);
}

#[test]
fn overlay_changes_land_in_the_upper_layer_only() {
    require_privileged!();
    let lower = temp_dir("lower");
    std::fs::write(lower.join("notes.md"), "v1").unwrap();
    std::fs::write(lower.join("old.md"), "古い").unwrap();
    chown_tree(&lower);
    let upper = temp_dir("upper");
    chown_tree(&upper);
    let work = temp_dir("work");

    let mut spec =
        sh("echo v2 > /data/p/notes.md && rm /data/p/old.md && echo new > /data/p/new.md");
    spec.mounts = vec![Mount {
        target: "/data/p".into(),
        source: MountSource::Overlay {
            lower: lower.clone(),
            upper: upper.clone(),
            work,
        },
        writable: true,
    }];
    let r = run(&sandbox(), spec);
    assert!(r.ok, "{}", r.stderr);

    // 下の層は変わらない
    assert_eq!(
        std::fs::read_to_string(lower.join("notes.md")).unwrap(),
        "v1"
    );
    assert!(lower.join("old.md").exists());
    // 変更は上の層に入る。削除は whiteout（デバイス番号 0/0 のキャラクタデバイス）
    assert_eq!(
        std::fs::read_to_string(upper.join("notes.md")).unwrap(),
        "v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(upper.join("new.md")).unwrap(),
        "new\n"
    );
    let whiteout = std::fs::symlink_metadata(upper.join("old.md")).unwrap();
    assert!(whiteout.file_type().is_char_device() && whiteout.rdev() == 0);
}
