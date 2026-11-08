//! 起動役の処理。自分自身に制限をかけてから対象のプログラムを exec する。

use crate::landlock_rules;
use crate::plan::{LaunchPlan, PlanLimits};
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

/// 計画どおりに自分自身へ制限をかけ、対象のプログラムを exec する。
///
/// 成功すると戻らない。戻ってきた場合は、制限の適用か exec に失敗している。
/// 制限を一部でも適用できなければ exec しない（制限なしでは動かさない）。
pub fn exec_sandboxed(plan: &LaunchPlan) -> Result<Infallible, String> {
    if !plan.program.is_absolute() {
        return Err("プログラムは絶対パスで指定してください".into());
    }

    set_parent_process_death_signal(Some(Signal::KILL))
        .map_err(|e| format!("親の終了を検知する設定ができません: {e}"))?;
    apply_limits(&plan.limits)?;
    landlock_rules::restrict_self(plan)?;
    // seccomp は最後に適用する（それまでの処理で使うシステムコールを拒否しないように）
    let programs = seccomp_filter::build(plan.deny_network)?;
    seccomp_filter::apply(&programs)?;

    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    let error = command.exec();
    Err(format!(
        "{} を実行できませんでした: {error}",
        plan.program.display()
    ))
}
