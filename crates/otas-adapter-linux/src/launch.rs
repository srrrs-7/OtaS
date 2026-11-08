//! 起動役の処理。自分自身に制限をかけてから対象のプログラムを exec する。

use crate::cgroup;
use crate::landlock_rules;
use crate::namespace;
use crate::plan::{LaunchPlan, PlanAccess, PlanFsRule, PlanLimits};
use crate::seccomp_filter;
use rustix::process::{Resource, Rlimit, Signal, set_parent_process_death_signal, setrlimit};
use std::convert::Infallible;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn set_limit(resource: Resource, value: u64) -> Result<(), String> {
    setrlimit(
        resource,
        Rlimit {
            current: Some(value),
            maximum: Some(value),
        },
    )
    .map_err(|e| format!("資源の上限（{resource:?}）を設定できません: {e}"))
}

fn apply_limits(limits: &PlanLimits) -> Result<(), String> {
    // コアダンプにはメモリの中身（秘密情報を含みうる）が残るので、常に無効にする
    set_limit(Resource::Core, 0)?;
    let optional = [
        (Resource::Cpu, limits.cpu_seconds),
        (Resource::As, limits.address_space_bytes),
        (Resource::Fsize, limits.file_size_bytes),
        (Resource::Nofile, limits.open_files),
    ];
    for (resource, value) in optional {
        if let Some(value) = value {
            set_limit(resource, value)?;
        }
    }
    Ok(())
}

fn death_signal() -> Result<(), String> {
    set_parent_process_death_signal(Some(Signal::KILL))
        .map_err(|e| format!("親の終了を検知する設定ができません: {e}"))
}

fn restrict(rules: &[PlanFsRule], deny_network: bool) -> Result<(), String> {
    landlock_rules::restrict_self(rules, deny_network)?;
    // seccomp は最後に適用する（それまでの処理で使うシステムコールを拒否しないように）
    let programs = seccomp_filter::build(deny_network)?;
    seccomp_filter::apply(&programs)
}

fn command(plan: &LaunchPlan) -> Command {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    command
}

/// 終了状態を終了コードにする（シグナルで終わった場合は 128 + シグナル番号）。
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// 計画どおりに自分自身へ制限をかけ、対象のプログラムを exec する（起動役の1段目）。
///
/// 成功すると戻らない。戻ってきた場合は、制限の適用か exec に失敗している。
/// 制限を一部でも適用できなければ exec しない（制限なしでは動かさない）。
///
/// 名前空間を使う場合は、名前空間を作ってから自分自身を `--pid1` で起動し（2段目。[`run_pid1`]）、
/// その終了を待って同じ終了コードで終わる。
pub fn exec_sandboxed(plan: &LaunchPlan) -> Result<Infallible, String> {
    if !plan.program.is_absolute() {
        return Err("プログラムは絶対パスで指定してください".into());
    }
    // 最初に cgroup に入る（以後の子孫はすべて、この cgroup の上限の中で動く）
    if let Some(dir) = &plan.cgroup {
        cgroup::join(dir)?;
    }
    death_signal()?;
    apply_limits(&plan.limits)?;

    if plan.namespace.is_some() {
        namespace::unshare_all()?;
        let me =
            std::env::current_exe().map_err(|e| format!("起動役のパスを取得できません: {e}"))?;
        let json = serde_json::to_string(plan).map_err(|e| e.to_string())?;
        let status = Command::new(me)
            .arg("--pid1")
            .arg(json)
            .env_clear()
            .status()
            .map_err(|e| format!("名前空間の中の起動役を起動できません: {e}"))?;
        std::process::exit(exit_code(status));
    }

    restrict(&plan.fs, plan.deny_network)?;
    let error = command(plan).exec();
    Err(format!(
        "{} を実行できませんでした: {error}",
        plan.program.display()
    ))
}

/// 名前空間の中の起動役（2段目）。PID 名前空間の PID 1 として動く。
///
/// 新しいルートを組み立てて一般ユーザーに切り替え、制限をかけてから、対象のプログラムを子（PID 2）として起動する。
/// 自分は init として残り、孤児になったプロセスを回収し、対象のプログラムが終わったら同じ終了コードで終わる。
/// PID 1 が終わると、名前空間の中の残りのプロセスはカーネルがすべて止める。
pub fn run_pid1(plan: &LaunchPlan) -> Result<Infallible, String> {
    use nix::errno::Errno;
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::Pid;

    let ns = plan
        .namespace
        .as_ref()
        .ok_or("名前空間の計画がありません")?;
    death_signal()?;
    namespace::build_root(ns, &plan.fs)?;
    // ユーザーを切り替えると親の終了の検知が解除されるので、設定し直す
    death_signal()?;

    let mut rules = plan.fs.clone();
    rules.push(PlanFsRule {
        path: "/tmp".into(),
        access: PlanAccess::ReadWrite,
        optional: false,
    });
    rules.push(PlanFsRule {
        path: "/proc".into(),
        access: PlanAccess::ReadOnly,
        optional: false,
    });
    for m in &ns.mounts {
        rules.push(PlanFsRule {
            path: m.target.clone(),
            access: if m.writable {
                PlanAccess::ReadWrite
            } else {
                PlanAccess::ReadOnly
            },
            optional: false,
        });
    }
    restrict(&rules, plan.deny_network)?;

    let mut cmd = command(plan);
    if plan.cwd.is_none() {
        cmd.current_dir("/");
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{} を実行できませんでした: {e}", plan.program.display()))?;
    let target = Pid::from_raw(child.id() as i32);
    drop(child);

    loop {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == target => std::process::exit(code),
            Ok(WaitStatus::Signaled(pid, signal, _)) if pid == target => {
                std::process::exit(128 + signal as i32)
            }
            // 孤児になった子孫の終了を回収する
            Ok(_) | Err(Errno::EINTR) => continue,
            Err(e) => return Err(format!("子プロセスを待てません: {e}")),
        }
    }
}
