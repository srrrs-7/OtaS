//! [`Sandbox`] の Linux 実装。

use crate::landlock_rules;
use crate::plan::{LaunchPlan, PlanAccess, PlanFsRule};
use otas_ports::{ExitInfo, NetPolicy, Sandbox, SandboxError, SandboxProcess, SandboxSpec};
use otas_types::{Guarantees, Level};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};

/// 起動役が制限の適用や exec に失敗したときの終了コード。
pub const LAUNCHER_FAILURE_EXIT_CODE: i32 = 125;

/// プログラムが動くために最低限必要なシステムのパス（読み取りと実行、または読み取りだけ）。
fn system_rules() -> Vec<PlanFsRule> {
    let rule = |path: &str, access| PlanFsRule {
        path: path.into(),
        access,
        optional: true,
    };
    vec![
        rule("/usr", PlanAccess::ReadExec),
        rule("/bin", PlanAccess::ReadExec),
        rule("/sbin", PlanAccess::ReadExec),
        rule("/lib", PlanAccess::ReadExec),
        rule("/lib64", PlanAccess::ReadExec),
        rule("/etc/ld.so.cache", PlanAccess::ReadOnly),
        rule("/dev/null", PlanAccess::ReadWrite),
        rule("/dev/zero", PlanAccess::ReadOnly),
        rule("/dev/urandom", PlanAccess::ReadOnly),
    ]
}

/// `..` や `.` を含まない絶対パスか。
fn is_clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// Landlock と seccomp によるサンドボックス。
#[derive(Debug, Clone)]
pub struct LinuxSandbox {
    launcher: PathBuf,
    system: Vec<PlanFsRule>,
}

impl LinuxSandbox {
    /// この環境で使えるか確かめて作る。`launcher` は `otas-sandbox-exec` のパス。
    pub fn detect(launcher: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let launcher = launcher.into();
        if !launcher.is_file() {
            return Err(SandboxError::Unsupported(format!(
                "起動役 {} が見つかりません",
                launcher.display()
            )));
        }
        landlock_rules::probe().map_err(SandboxError::Unsupported)?;
        Ok(Self {
            launcher,
            system: system_rules(),
        })
    }

    fn plan(&self, spec: &SandboxSpec) -> Result<LaunchPlan, SandboxError> {
        if !is_clean_absolute(&spec.program) {
            return Err(SandboxError::InvalidSpec(format!(
                "プログラムは `..` を含まない絶対パスで指定してください: {}",
                spec.program.display()
            )));
        }
        let mut fs = self.system.clone();
        for rule in &spec.fs {
            if !is_clean_absolute(&rule.path) {
                return Err(SandboxError::InvalidSpec(format!(
                    "パスは `..` を含まない絶対パスで指定してください: {}",
                    rule.path.display()
                )));
            }
            fs.push(PlanFsRule {
                path: rule.path.clone(),
                access: rule.access.into(),
                optional: false,
            });
        }
        Ok(LaunchPlan {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            fs,
            deny_network: matches!(spec.net, NetPolicy::Deny),
            limits: spec.limits.into(),
        })
    }
}

impl Sandbox for LinuxSandbox {
    fn guarantees(&self) -> Guarantees {
        Guarantees {
            fs_isolation: Level::Strong,
            net_isolation: Level::Strong,
            syscall_filter: Level::Weak,
            snapshot: Level::Absent,
        }
    }

    fn spawn(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxProcess>, SandboxError> {
        let plan = self.plan(&spec)?;
        let plan =
            serde_json::to_string(&plan).map_err(|e| SandboxError::InvalidSpec(e.to_string()))?;
        let child = Command::new(&self.launcher)
            .arg(plan)
            // 起動役にも親の環境変数は渡さない
            .env_clear()
            .stdin(spec.stdin)
            .stdout(spec.stdout)
            .stderr(spec.stderr)
            .spawn()?;
        Ok(Box::new(LinuxProcess { child }))
    }
}

struct LinuxProcess {
    child: Child,
}

fn exit_info(status: ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
    }
}

impl SandboxProcess for LinuxProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn wait(&mut self) -> Result<ExitInfo, SandboxError> {
        Ok(exit_info(self.child.wait()?))
    }

    fn try_wait(&mut self) -> Result<Option<ExitInfo>, SandboxError> {
        Ok(self.child.try_wait()?.map(exit_info))
    }

    fn kill(&mut self) -> Result<(), SandboxError> {
        Ok(self.child.kill()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_absolute_paths_only() {
        assert!(is_clean_absolute(Path::new("/data/a")));
        assert!(!is_clean_absolute(Path::new("data/a")));
        assert!(!is_clean_absolute(Path::new("/data/../etc")));
        // `.` は Path::components が取り除く（/data/a と同じ場所を指すので害はない）
        assert!(is_clean_absolute(Path::new("/data/./a")));
    }
}
