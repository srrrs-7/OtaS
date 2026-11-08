//! OverlayFS と Btrfs を使う作業領域のテスト。
//!
//! 実際に OverlayFS をマウントするので root が必要。特権を足したテスト用のコンテナ（compose の
//! `dev-privileged`）で `OTAS_TEST_PRIVILEGED=1` のときだけ動かす。Btrfs のテストは、さらに
//! `OTAS_TEST_BTRFS_DIR`（Btrfs をマウントしたディレクトリ）があるときだけ動かす。

#![cfg(target_os = "linux")]

use nix::mount::{MsFlags, mount, umount};
use otas_ports::{Change, ChangeKind, CommitOutcome, MountSource, ViewSpec, Workspace};
use otas_types::{LabelSet, Level, TaskId};
use otas_workspace::{FsStore, StoreOptions, VersionBackend, ViewMode};
use std::fs;
use std::path::{Path, PathBuf};

fn privileged() -> bool {
    std::env::var("OTAS_TEST_PRIVILEGED").as_deref() == Ok("1")
}

/// 正本に projects/a/{notes.md, old.md, dir/x.md, dir/y.md} を置く。
fn seed(root: &Path) {
    let a = root.join("live/projects/a");
    fs::create_dir_all(a.join("dir")).unwrap();
    fs::write(a.join("notes.md"), "v1").unwrap();
    fs::write(a.join("old.md"), "古い").unwrap();
    fs::write(a.join("dir/x.md"), "x").unwrap();
    fs::write(a.join("dir/y.md"), "y").unwrap();
}

/// 作業領域を OverlayFS としてマウントし、`edit` を実行してから外す（エージェントの代わり）。
/// `userxattr` は root なしの方式（ユーザー名前空間）と同じく、印を user.overlay.* に付ける。
fn edit_through_overlay(source: &MountSource, userxattr: bool, edit: impl FnOnce(&Path)) {
    let MountSource::Overlay { lower, upper, work } = source else {
        panic!("OverlayFS の作業領域ではない: {source:?}");
    };
    let merged = std::env::temp_dir().join(format!("otas-merged-{}", TaskId::new()));
    fs::create_dir_all(&merged).unwrap();
    let options = format!(
        "lowerdir={},upperdir={},workdir={},redirect_dir=nofollow,metacopy=off,index=off,xino=off{}",
        lower.display(),
        upper.display(),
        work.display(),
        if userxattr { ",userxattr" } else { "" }
    );
    mount(
        Some("overlay"),
        &merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .unwrap();
    edit(&merged);
    umount(&merged).unwrap();
    fs::remove_dir(&merged).unwrap();
}

fn write_view(path: &str) -> ViewSpec {
    ViewSpec {
        read: vec![],
        write: vec![path.to_string()],
    }
}

fn exercise(root: PathBuf, versions: VersionBackend, userxattr: bool) {
    if versions == VersionBackend::Btrfs {
        // Btrfs では、正本はサブボリュームとして用意しておく（普通のディレクトリは受け付けない）
        fs::create_dir_all(&root).unwrap();
        let ok = std::process::Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(root.join("live"))
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    seed(&root);
    let store = FsStore::open_with(
        &root,
        StoreOptions {
            view: ViewMode::Overlay,
            versions,
            owner: None,
        },
    )
    .unwrap();

    let task = TaskId::new();
    let view = store.begin(task, &write_view("/data/projects/a")).unwrap();
    edit_through_overlay(&view.entries[0].source, userxattr, |merged| {
        fs::write(merged.join("notes.md"), "v2").unwrap();
        fs::remove_file(merged.join("old.md")).unwrap();
        fs::write(merged.join("new.md"), "新規").unwrap();
        // ディレクトリを丸ごと消して作り直す（上の層では opaque なディレクトリになる）
        fs::remove_dir_all(merged.join("dir")).unwrap();
        fs::create_dir(merged.join("dir")).unwrap();
        fs::write(merged.join("dir/z.md"), "z").unwrap();
    });

    let change = |path: &str, kind| Change {
        path: format!("/data/projects/a/{path}"),
        kind,
    };
    let diff = store.diff(task).unwrap();
    assert_eq!(
        diff,
        vec![
            change("dir/x.md", ChangeKind::Deleted),
            change("dir/y.md", ChangeKind::Deleted),
            change("dir/z.md", ChangeKind::Added),
            change("new.md", ChangeKind::Added),
            change("notes.md", ChangeKind::Modified),
            change("old.md", ChangeKind::Deleted),
        ]
    );
    // コミットするまで正本は変わらない
    let live = store.live_dir().join("projects/a");
    assert_eq!(fs::read_to_string(live.join("notes.md")).unwrap(), "v1");

    let outcome = store.commit(task, &LabelSet::empty()).unwrap();
    assert!(matches!(outcome, CommitOutcome::Applied { version: 2, .. }));
    assert_eq!(fs::read_to_string(live.join("notes.md")).unwrap(), "v2");
    assert_eq!(fs::read_to_string(live.join("new.md")).unwrap(), "新規");
    assert_eq!(fs::read_to_string(live.join("dir/z.md")).unwrap(), "z");
    assert!(!live.join("old.md").exists());
    assert!(!live.join("dir/x.md").exists());
    // 過去の版は変わらない
    let v1 = root.join("versions/1/projects/a/notes.md");
    assert_eq!(fs::read_to_string(&v1).unwrap(), "v1");

    // 巻き戻し
    assert_eq!(store.rollback(1).unwrap(), 3);
    assert_eq!(fs::read_to_string(live.join("notes.md")).unwrap(), "v1");
    assert!(live.join("old.md").exists());
}

#[test]
fn overlay_workspaces_commit_edits_deletions_and_replaced_directories() {
    if !privileged() {
        eprintln!("特権を足したテスト用のコンテナでだけ動かします（OTAS_TEST_PRIVILEGED=1）");
        return;
    }
    let root = std::env::temp_dir().join(format!("otas-ovl-{}", TaskId::new()));
    exercise(root, VersionBackend::Hardlink, false);
}

#[test]
fn btrfs_versions_are_read_only_snapshots() {
    let Ok(dir) = std::env::var("OTAS_TEST_BTRFS_DIR") else {
        eprintln!("Btrfs をマウントしたテスト用のコンテナでだけ動かします（OTAS_TEST_BTRFS_DIR）");
        return;
    };
    let root = PathBuf::from(dir).join(format!("otas-btrfs-{}", TaskId::new()));
    exercise(root.clone(), VersionBackend::Btrfs, false);

    let store = FsStore::open_with(
        &root,
        StoreOptions {
            view: ViewMode::Overlay,
            versions: VersionBackend::Btrfs,
            owner: None,
        },
    )
    .unwrap();
    assert_eq!(store.snapshot_level(), Level::Strong);
    // 版は読み取り専用のスナップショットなので、root でも書き換えられない
    let err = fs::write(root.join("versions/1/projects/a/notes.md"), "改ざん").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(30), "EROFS のはず: {err}");
}

#[test]
fn overlay_changes_marked_with_user_xattrs_are_read_too() {
    // root なしの方式では、opaque の印が user.overlay.opaque に付く
    if !privileged() {
        eprintln!("特権を足したテスト用のコンテナでだけ動かします（OTAS_TEST_PRIVILEGED=1）");
        return;
    }
    let root = std::env::temp_dir().join(format!("otas-ws-userxattr-{}", TaskId::new()));
    exercise(root, VersionBackend::Hardlink, true);
}
