//! 名前空間の準備。起動役が root として動いている間に行い、終わったら一般ユーザーに切り替える。
//!
//! root なしの方式（rootless）では、最初にユーザー名前空間を作り、その中の root（外から見るとデーモンと同じユーザー）
//! として同じ準備をする。最後はユーザーを切り替える代わりに、ケーパビリティをすべて捨てる。
//!
//! 1. マウント・ネットワーク・IPC・UTS・PID の名前空間を新しく作る（ネットワークは空なので外に出られない）
//! 2. 空の tmpfs を新しいルートにして、許可したものだけを置く（PID 名前空間の中だけが見える `/proc` も置く）
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

/// 元のマウントに付いている制限。ユーザー名前空間の中では、これらを外す remount は拒否される（ロックされている）ので、
/// そのまま引き継ぐ。root の方式でも、元より緩めないために引き継ぐ。
fn locked_flags(src: &Path) -> MsFlags {
    use nix::sys::statvfs::{FsFlags, statvfs};
    let Ok(st) = statvfs(src) else {
        return MsFlags::empty();
    };
    let f = st.flags();
    let mut flags = MsFlags::empty();
    for (from, to) in [
        (FsFlags::ST_RDONLY, MsFlags::MS_RDONLY),
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        (FsFlags::ST_RELATIME, MsFlags::MS_RELATIME),
    ] {
        if f.contains(from) {
            flags |= to;
        }
    }
    flags
}

fn bind(src: &Path, dst: &Path, writable: bool, device: bool) -> Result<(), String> {
    let inherited = locked_flags(src);
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
    let mut flags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_NOSUID | inherited;
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

fn mount_one(root: &Path, m: &PlanMount, rootless: bool) -> Result<(), String> {
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
            // ユーザー名前空間の中では trusted.* の拡張属性を使えないので、user.overlay.* を使う（userxattr）。
            // redirect_dir=off は「作らないが、たどる」の意味で userxattr と両立しないため、nofollow（作らず、たどらない）にする
            let data = format!(
                "lowerdir={},upperdir={},workdir={},redirect_dir=nofollow,metacopy=off,index=off,xino=off{}",
                lower.display(),
                upper.display(),
                work.display(),
                if rootless { ",userxattr" } else { "" }
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

/// ユーザー名前空間を作れない理由の説明（Ubuntu 23.10 以降の AppArmor の制限など）。
fn userns_hint() -> String {
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    if restricted {
        "。AppArmor が特権のないユーザー名前空間を制限しています\
         （kernel.apparmor_restrict_unprivileged_userns=1）。\
         packaging/apparmor/otas-sandbox-exec のプロファイルで起動役に userns を許可してください"
            .into()
    } else {
        String::new()
    }
}

/// 自分の uid / gid を、新しいユーザー名前空間の中の 0 に対応づける。
fn map_self(uid: u32, gid: u32) -> Result<(), String> {
    // 補助グループを変えられないようにしてからでないと、gid_map を書けない
    fs::write("/proc/self/setgroups", "deny")
        .map_err(|e| fail("setgroups を無効にできません", e))?;
    fs::write("/proc/self/uid_map", format!("0 {uid} 1"))
        .map_err(|e| format!("uid_map を書けません: {e}{}", userns_hint()))?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1"))
        .map_err(|e| format!("gid_map を書けません: {e}{}", userns_hint()))
}

/// root なしの方式を使えるか確かめる（起動役の `--probe-userns`。確かめた後のプロセスは捨てる）。
pub(crate) fn probe_userns() -> Result<(), String> {
    let (uid, gid) = (nix::unistd::geteuid(), nix::unistd::getegid());
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .map_err(|e| format!("ユーザー名前空間を作れません: {e}{}", userns_hint()))?;
    map_self(uid.as_raw(), gid.as_raw())
}

/// 名前空間を作る（起動役の1段目）。PID 名前空間は、この後に作る子プロセスから有効になる。
///
/// `rootless` なら、ユーザー名前空間も作り、その中の root になる（外から見るとデーモンと同じユーザー）。
pub(crate) fn unshare_all(rootless: bool) -> Result<(), String> {
    let mut flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWPID;
    let owner = (nix::unistd::geteuid(), nix::unistd::getegid());
    if rootless {
        // カーネルはユーザー名前空間を先に作り、残りの名前空間はその持ち物になる
        flags |= CloneFlags::CLONE_NEWUSER;
    } else if !nix::unistd::getuid().is_root() {
        return Err("名前空間のサンドボックスは root で起動してください".into());
    }
    unshare(flags).map_err(|e| {
        let hint = if rootless {
            userns_hint()
        } else {
            String::new()
        };
        format!("名前空間を作れません: {e}{hint}")
    })?;
    if rootless {
        map_self(owner.0.as_raw(), owner.1.as_raw())?;
    }
    // ここから先のマウントが元の名前空間に漏れないようにする
    mount(NONE, "/", NONE, MsFlags::MS_REC | MsFlags::MS_PRIVATE, NONE)
        .map_err(|e| fail("マウントの伝播を止められません", e))
}

/// 新しいルートを組み立てて移り、一般ユーザーに切り替える（起動役の2段目。PID 名前空間の PID 1 として動く）。
pub(crate) fn build_root(plan: &NamespacePlan, fs_rules: &[PlanFsRule]) -> Result<(), String> {
    if !nix::unistd::getuid().is_root() {
        return Err("名前空間のサンドボックスは root で起動してください".into());
    }
    if !plan.rootless && (plan.uid == 0 || plan.gid == 0) {
        return Err("タスクを root として動かすことはできません".into());
    }

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

    // この PID 名前空間のプロセスだけが見える /proc
    let proc_dir = root.join("proc");
    fs::create_dir_all(&proc_dir).map_err(|e| fail("/proc を作れません", e))?;
    mount(
        Some("proc"),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        NONE,
    )
    .map_err(|e| fail("/proc をマウントできません", e))?;

    for rule in fs_rules {
        expose(root, rule)?;
    }
    for m in &plan.mounts {
        mount_one(root, m, plan.rootless)?;
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

    if plan.rootless {
        // ユーザー名前空間の中には uid 0 しかないので、切り替える代わりにケーパビリティを捨てる
        return drop_capabilities();
    }

    // 一般ユーザーに切り替える。uid がすべて 0 以外になるので、ケーパビリティはすべて失われる
    setgroups(&[]).map_err(|e| fail("補助グループを外せません", e))?;
    let gid = Gid::from_raw(plan.gid);
    let uid = Uid::from_raw(plan.uid);
    setresgid(gid, gid, gid).map_err(|e| fail("グループを切り替えられません", e))?;
    setresuid(uid, uid, uid).map_err(|e| fail("ユーザーを切り替えられません", e))?;
    Ok(())
}

/// ケーパビリティをすべて捨て、二度と得られないようにする（root なしの方式。ユーザー名前空間の中の uid 0 のまま動く）。
///
/// uid 0 のプロセスは exec のたびにケーパビリティを得直すので、次の順にすべて塞ぐ。
/// 1. securebits：root であることを理由にケーパビリティを与えない（NOROOT など）を設定してロックする
/// 2. bounding set を空にする（exec で得られる上限）
/// 3. ambient を空にする
/// 4. effective・permitted・inheritable を空にする
fn drop_capabilities() -> Result<(), String> {
    use rustix::thread::{
        CapabilitiesSecureBits as Bits, CapabilitySet, CapabilitySets,
        clear_ambient_capability_set, remove_capability_from_bounding_set, set_capabilities,
        set_capabilities_secure_bits,
    };
    set_capabilities_secure_bits(
        Bits::NO_ROOT
            | Bits::NO_ROOT_LOCKED
            | Bits::NO_SETUID_FIXUP
            | Bits::NO_SETUID_FIXUP_LOCKED
            | Bits::KEEP_CAPS_LOCKED
            | Bits::NO_CAP_AMBIENT_RAISE
            | Bits::NO_CAP_AMBIENT_RAISE_LOCKED,
    )
    .map_err(|e| fail("securebits を設定できません", e))?;
    for cap in 0..64 {
        match remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << cap)) {
            Ok(()) => {}
            // カーネルが知らない番号まで来た
            Err(rustix::io::Errno::INVAL) => break,
            Err(e) => return Err(fail(format!("ケーパビリティ {cap} を捨てられません"), e)),
        }
    }
    clear_ambient_capability_set()
        .map_err(|e| fail("ambient のケーパビリティを捨てられません", e))?;
    set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(|e| fail("ケーパビリティを捨てられません", e))
}
