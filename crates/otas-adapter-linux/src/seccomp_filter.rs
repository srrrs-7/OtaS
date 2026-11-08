//! seccomp による危険なシステムコールの拒否。
//!
//! 危険なものを列挙して拒否する方式（拒否リスト）。許可リスト方式より抜けが出やすいので、
//! 保証は Weak として申告する。

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;

/// 常に拒否する（EPERM を返す）システムコール。
fn denied_syscalls() -> Vec<i64> {
    let list = vec![
        // 他のプロセスの覗き見・操作
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        // 名前空間・マウント（隔離の外に出る足がかりになる）
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        // ファイルハンドル経由のアクセス（パスによる制限を迂回できる）
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        // カーネルへの直接の作用
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_syslog,
        libc::SYS_vhangup,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        // 鍵の保管庫（他のプロセスの秘密に触れうる）
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        // io_uring（seccomp の引数検査を迂回して操作できる）
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        // ファイルシステム全体の監視
        libc::SYS_fanotify_init,
    ];
    // x86_64 だけにある、I/O ポートや古いライブラリ読み込みのシステムコール
    #[cfg(target_arch = "x86_64")]
    let list = {
        let mut list = list;
        list.extend([libc::SYS_iopl, libc::SYS_ioperm, libc::SYS_uselib]);
        list
    };
    list
}

/// `clone` のフラグのうち、名前空間を作るもの。
const NAMESPACE_FLAGS: [libc::c_int; 7] = [
    libc::CLONE_NEWNS,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
    libc::CLONE_NEWCGROUP,
];

fn err(e: impl std::fmt::Display) -> String {
    format!("seccomp フィルタを作れません: {e}")
}

/// 現在のアーキテクチャ向けのフィルタを作る。
///
/// 2つのフィルタを返す。両方を順に適用する。
/// - 拒否リスト（EPERM）
/// - `clone3` を ENOSYS にする（引数が構造体で検査できないため。libc は `clone` に切り替える）
pub(crate) fn build(deny_network: bool) -> Result<Vec<BpfProgram>, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(err)?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = denied_syscalls()
        .into_iter()
        .map(|nr| (nr, Vec::new()))
        .collect();

    if deny_network {
        // ソケットは作らせない。ブローカーとの通信は親から渡された stdin を使う
        rules.insert(libc::SYS_socket, Vec::new());
    }

    // 名前空間を作る clone を拒否する（フラグのどれか1つでも立っていれば一致）
    let clone_rules = NAMESPACE_FLAGS
        .iter()
        .map(|flag| {
            let flag = *flag as u64;
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(flag),
                flag,
            )
            .and_then(|c| SeccompRule::new(vec![c]))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    rules.insert(libc::SYS_clone, clone_rules);

    let deny = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(err)?;

    let clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(err)?;

    Ok(vec![
        BpfProgram::try_from(deny).map_err(err)?,
        BpfProgram::try_from(clone3).map_err(err)?,
    ])
}

/// フィルタを自分自身に適用する。
pub(crate) fn apply(programs: &[BpfProgram]) -> Result<(), String> {
    for program in programs {
        seccompiler::apply_filter(program)
            .map_err(|e| format!("seccomp フィルタを適用できません: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_build_for_this_architecture() {
        let programs = build(true).unwrap();
        assert_eq!(programs.len(), 2);
        assert!(programs.iter().all(|p| !p.is_empty()));
    }

    #[test]
    fn denylist_has_no_duplicates() {
        let mut list = denied_syscalls();
        let n = list.len();
        list.sort_unstable();
        list.dedup();
        assert_eq!(list.len(), n);
    }
}
