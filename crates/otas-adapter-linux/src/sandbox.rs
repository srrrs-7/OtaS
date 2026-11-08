//! [`Sandbox`] の Linux 実装。

use crate::cgroup::CgroupManager;
use crate::landlock_rules;
use crate::plan::{LaunchPlan, NamespacePlan, PlanAccess, PlanFsRule, PlanMount, PlanMountSource};
use otas_ports::{
    ExitInfo, MountSource, NetPolicy, Sandbox, SandboxError, SandboxProcess, SandboxSpec,
};
use otas_types::{Guarantees, Level, TaskId};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;

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

/// 名前空間を使う方式の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceConfig {
    /// タスクを動かすユーザー（0 以外）。
    pub uid: u32,
    /// タスクを動かすグループ（0 以外）。
    pub gid: u32,
    /// タスクごとの新しいルートを組み立てるディレクトリ。
    pub run_dir: PathBuf,
}

/// Landlock と seccomp によるサンドボックス（名前空間を使う方式も選べる）。
#[derive(Debug, Clone)]
pub struct LinuxSandbox {
    launcher: PathBuf,
    system: Vec<PlanFsRule>,
    namespace: Option<NamespaceConfig>,
    rootless: bool,
    cgroups: Option<Arc<CgroupManager>>,
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
            namespace: None,
            rootless: false,
            cgroups: None,
        })
    }

    /// root なしで名前空間を使う方式で作る（ユーザー名前空間）。デーモンは一般ユーザーで動かせる。
    ///
    /// タスクは外から見るとデーモンと同じユーザーで動くが、ケーパビリティはすべて捨てる。
    /// 許可していないパスは、root の方式と同じくサンドボックスの中に存在しない。
    /// この環境でユーザー名前空間を作れなければ（AppArmor の制限など）、理由を添えて断る。
    pub fn detect_rootless(
        launcher: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let mut sandbox = Self::detect(launcher)?;
        let probe = Command::new(&sandbox.launcher)
            .arg("--probe-userns")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()?;
        if !probe.status.success() {
            return Err(SandboxError::Unsupported(format!(
                "root なしの方式を使えません: {}",
                String::from_utf8_lossy(&probe.stderr).trim()
            )));
        }
        let run_dir = run_dir.into();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&run_dir)?;
        sandbox.namespace = Some(NamespaceConfig {
            uid: 0,
            gid: 0,
            run_dir,
        });
        sandbox.rootless = true;
        Ok(sandbox)
    }

    /// cgroups v2 で資源の上限（メモリ・プロセス数・CPU）をかけられるようにする。
    /// `base` はデーモンに任された cgroup（systemd の `Delegate=yes` など）。
    pub fn with_cgroups(mut self, base: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        self.cgroups = Some(Arc::new(CgroupManager::open(base)?));
        Ok(self)
    }

    /// 名前空間を使う方式で作る。デーモンが root で動いている必要がある。
    pub fn detect_namespaced(
        launcher: impl Into<PathBuf>,
        config: NamespaceConfig,
    ) -> Result<Self, SandboxError> {
        let mut sandbox = Self::detect(launcher)?;
        if !rustix::process::getuid().is_root() {
            return Err(SandboxError::Unsupported(
                "名前空間を使う方式は、デーモンを root で動かす必要があります".into(),
            ));
        }
        if config.uid == 0 || config.gid == 0 {
            return Err(SandboxError::InvalidSpec(
                "タスクを root として動かすことはできません（uid と gid は 0 以外）".into(),
            ));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.run_dir)?;
        sandbox.namespace = Some(config);
        Ok(sandbox)
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
        let namespace = match &self.namespace {
            None if !spec.mounts.is_empty() => {
                return Err(SandboxError::InvalidSpec(
                    "このサンドボックスは別のパスに見せること（mounts）に対応していません".into(),
                ));
            }
            None => None,
            Some(config) => {
                let mut mounts = Vec::new();
                for m in &spec.mounts {
                    if !is_clean_absolute(&m.target) || m.target == Path::new("/") {
                        return Err(SandboxError::InvalidSpec(format!(
                            "マウント先は `..` を含まない絶対パスで指定してください: {}",
                            m.target.display()
                        )));
                    }
                    let source = match &m.source {
                        MountSource::Path(path) => PlanMountSource::Bind { path: path.clone() },
                        MountSource::Overlay { lower, upper, work } => PlanMountSource::Overlay {
                            lower: lower.clone(),
                            upper: upper.clone(),
                            work: work.clone(),
                        },
                    };
                    mounts.push(PlanMount {
                        target: m.target.clone(),
                        source,
                        writable: m.writable,
                    });
                }
                Some(NamespacePlan {
                    root: config.run_dir.join(TaskId::new().0.simple().to_string()),
                    uid: config.uid,
                    gid: config.gid,
                    mounts,
                    rootless: self.rootless,
                })
            }
        };
        Ok(LaunchPlan {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            fs,
            deny_network: matches!(spec.net, NetPolicy::Deny),
            limits: spec.limits.into(),
            namespace,
            cgroup: None,
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

    fn supports_mounts(&self) -> bool {
        self.namespace.is_some()
    }

    fn spawn(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxProcess>, SandboxError> {
        let mut plan = self.plan(&spec)?;
        // cgroups が必要な上限が指定されているのに使えなければ、上限なしでは動かさない
        if spec.limits.needs_cgroup() && self.cgroups.is_none() {
            return Err(SandboxError::InvalidSpec(
                "メモリ・プロセス数・CPU の上限には cgroups の設定が必要です".into(),
            ));
        }
        let cgroup = match &self.cgroups {
            Some(manager) => {
                let dir = manager.create(&spec.limits)?;
                plan.cgroup = Some(dir.clone());
                Some((manager.clone(), dir))
            }
            None => None,
        };
        // 新しいルートを組み立てる空のディレクトリ。名前空間が消えれば空に戻るので、終了後に消す
        let cleanup = match &plan.namespace {
            Some(ns) => {
                std::fs::DirBuilder::new().mode(0o700).create(&ns.root)?;
                Some(ns.root.clone())
            }
            None => None,
        };
        let plan =
            serde_json::to_string(&plan).map_err(|e| SandboxError::InvalidSpec(e.to_string()))?;
        let child = Command::new(&self.launcher)
            .arg(plan)
            // 起動役にも親の環境変数は渡さない
            .env_clear()
            .stdin(spec.stdin)
            .stdout(spec.stdout)
            .stderr(spec.stderr)
            .spawn()
            .inspect_err(|_| {
                if let Some((manager, dir)) = &cgroup {
                    manager.remove(dir);
                }
            })?;
        Ok(Box::new(LinuxProcess {
            child,
            cleanup,
            cgroup,
        }))
    }
}

struct LinuxProcess {
    child: Child,
    cleanup: Option<PathBuf>,
    cgroup: Option<(Arc<CgroupManager>, PathBuf)>,
}

impl LinuxProcess {
    fn finished(&mut self) {
        if let Some(dir) = self.cleanup.take() {
            let _ = std::fs::remove_dir(dir);
        }
        if let Some((manager, dir)) = self.cgroup.take() {
            manager.remove(&dir);
        }
    }
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
        let info = exit_info(self.child.wait()?);
        self.finished();
        Ok(info)
    }

    fn try_wait(&mut self) -> Result<Option<ExitInfo>, SandboxError> {
        let info = self.child.try_wait()?.map(exit_info);
        if info.is_some() {
            self.finished();
        }
        Ok(info)
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
