//! 名前空間の準備。起動役が root として動いている間に行い、終わったら一般ユーザーに切り替える。
//!
//! 1. マウント・ネットワーク・IPC・UTS の名前空間を新しく作る（ネットワークは空なので外に出られない）
//! 2. 空の tmpfs を新しいルートにして、許可したものだけを置く
//!    - `fs` の規則のパスは同じパスで（シンボリックリンクはリンクとして）
//!    - `mounts` は指定したパスに（bind mount または OverlayFS）
//!    - `/tmp` はタスク専用の tmpfs
//! 3. `pivot_root` で新しいルートに移り、元のルートを切り離す。ルートは読み取り専用にする
//! 4. 補助グループを空にして、`uid` / `gid` に切り替える（ケーパビリティはすべて失われる）
//!
//! 許可していないパス（`/etc/passwd` や他のユーザーのデータ）は、新しいルートの中に存在しない。

use crate::plan::{NamespacePlan, PlanAccess, PlanFsRule, PlanMount, PlanMountSource};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::unistd::{Gid, Uid, chdir, pivot_root, setgroups, setresgid, setresuid};
use std::fmt::Display;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

const NONE: Option<&str> = None;

fn fail(what: impl Display, e: impl Display) -> String {
    format!("{what}: {e}")
}

/// ホストのパスを、新しいルートの中のパスにする。
fn inside(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

fn create_mount_point(dst: &Path, is_dir: bool) -> Result<(), String> {
    if is_dir {
        fs::create_dir_all(dst).map_err(|e| fail(format!("{} を作れません", dst.display()), e))
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| fail(format!("{} を作れません", parent.display()), e))?;
        }
        if !dst.exists() {
            fs::File::create(dst)
                .map_err(|e| fail(format!("{} を作れません", dst.display()), e))?;
        }
        Ok(())
    }
}

fn bind(src: &Path, dst: &Path, writable: bool, device: bool) -> Result<(), String> {
    mount(
        Some(src),
        dst,
        NONE,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    )
    .map_err(|e| {
        fail(
            format!("{} を {} に見せられません", src.display(), dst.display()),
            e,
        )
    })?;
    let mut flags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_NOSUID;
    if !device {
        flags |= MsFlags::MS_NODEV;
    }
    if !writable {
        flags |= MsFlags::MS_RDONLY;
    }
    mount(NONE, dst, NONE, flags, NONE)
        .map_err(|e| fail(format!("{} の権限を設定できません", dst.display()), e))
}

/// `fs` の規則のパスを、同じパスで見せる。
fn expose(root: &Path, rule: &PlanFsRule) -> Result<(), String> {
    let meta = match fs::symlink_metadata(&rule.path) {
        Ok(m) => m,
        Err(_) if rule.optional => return Ok(()),
        Err(e) => return Err(fail(format!("{} を開けません", rule.path.display()), e)),
    };
    let dst = inside(root, &rule.path);
    if meta.file_type().is_symlink() {
        // /bin → usr/bin のようなリンクは、リンクとして作る（リンク先は別の規則で見せる）
        let target = fs::read_link(&rule.path)
            .map_err(|e| fail(format!("{} を読めません", rule.path.display()), e))?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| fail("ディレクトリを作れません", e))?;
        }
        if fs::symlink_metadata(&dst).is_err() {
            std::os::unix::fs::symlink(&target, &dst)
                .map_err(|e| fail(format!("{} を作れません", dst.display()), e))?;
        }
        return Ok(());
    }
    create_mount_point(&dst, meta.is_dir())?;
    let device = meta.file_type().is_char_device() || meta.file_type().is_block_device();
    bind(
        &rule.path,
        &dst,
        rule.access == PlanAccess::ReadWrite,
        device,
    )
}

fn mount_one(root: &Path, m: &PlanMount) -> Result<(), String> {
    let dst = inside(root, &m.target);
    match &m.source {
        PlanMountSource::Bind { path } => {
            let is_dir = fs::metadata(path)
                .map_err(|e| fail(format!("{} を開けません", path.display()), e))?
                .is_dir();
            create_mount_point(&dst, is_dir)?;
            bind(path, &dst, m.writable, false)
        }
        PlanMountSource::Overlay { lower, upper, work } => {
            for p in [lower, upper, work] {
                let s = p.to_string_lossy();
                if s.contains(',') || s.contains(':') {
                    return Err(format!("OverlayFS に渡せないパスです: {s}"));
                }
            }
            create_mount_point(&dst, true)?;
            // 変更をすべて上の層に実体として残す設定にする（差分を正しく読むため）
            let data = format!(
                "lowerdir={},upperdir={},workdir={},redirect_dir=off,metacopy=off,index=off,xino=off",
                lower.display(),
                upper.display(),
                work.display()
            );
            mount(
                Some("overlay"),
                &dst,
                Some("overlay"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
                Some(data.as_str()),
            )
            .map_err(|e| {
                fail(
                    format!("{} に OverlayFS をマウントできません", dst.display()),
                    e,
                )
            })
        }
    }
}

/// 名前空間に入り、新しいルートを組み立てて移り、一般ユーザーに切り替える。
pub(crate) fn enter(plan: &NamespacePlan, fs_rules: &[PlanFsRule]) -> Result<(), String> {
    if !nix::unistd::getuid().is_root() {
        return Err("名前空間のサンドボックスは root で起動してください".into());
    }
    if plan.uid == 0 || plan.gid == 0 {
        return Err("タスクを root として動かすことはできません".into());
    }

    unshare(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWNET
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUTS,
    )
    .map_err(|e| fail("名前空間を作れません", e))?;
    // ここから先のマウントが元の名前空間に漏れないようにする
    mount(NONE, "/", NONE, MsFlags::MS_REC | MsFlags::MS_PRIVATE, NONE)
        .map_err(|e| fail("マウントの伝播を止められません", e))?;

    let root = &plan.root;
    mount(
        Some("tmpfs"),
        root,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0755"),
    )
    .map_err(|e| fail("新しいルートを作れません", e))?;

    // /tmp を先に作る（/tmp の下にある規則のパスが、後から tmpfs で隠れないように）
    let tmp = root.join("tmp");
    fs::create_dir_all(&tmp).map_err(|e| fail("/tmp を作れません", e))?;
    mount(
        Some("tmpfs"),
        &tmp,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777"),
    )
    .map_err(|e| fail("/tmp をマウントできません", e))?;

    for rule in fs_rules {
        expose(root, rule)?;
    }
    for m in &plan.mounts {
        mount_one(root, m)?;
    }

    let old = root.join(".oldroot");
    fs::create_dir(&old).map_err(|e| fail("一時ディレクトリを作れません", e))?;
    pivot_root(root, &old).map_err(|e| fail("新しいルートに移れません", e))?;
    chdir("/").map_err(|e| fail("ルートに移れません", e))?;
    umount2("/.oldroot", MntFlags::MNT_DETACH)
        .map_err(|e| fail("元のルートを切り離せません", e))?;
    fs::remove_dir("/.oldroot").map_err(|e| fail("一時ディレクトリを消せません", e))?;
    mount(
        NONE,
        "/",
        NONE,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        NONE,
    )
    .map_err(|e| fail("ルートを読み取り専用にできません", e))?;

    // 一般ユーザーに切り替える。uid がすべて 0 以外になるので、ケーパビリティはすべて失われる
    setgroups(&[]).map_err(|e| fail("補助グループを外せません", e))?;
    let gid = Gid::from_raw(plan.gid);
    let uid = Uid::from_raw(plan.uid);
    setresgid(gid, gid, gid).map_err(|e| fail("グループを切り替えられません", e))?;
    setresuid(uid, uid, uid).map_err(|e| fail("ユーザーを切り替えられません", e))?;
    Ok(())
}
