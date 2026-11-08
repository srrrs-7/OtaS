//! cgroups v2 による資源の上限（メモリ・プロセス数・CPU）。
//!
//! デーモンには、cgroups の一部（サブツリー）を任せてもらう前提で動く。
//! 本番では systemd のユニットで `Delegate=yes` を指定し、そのユニットの cgroup を `base` にする。
//!
//! ```text
//! <base>/                    デーモンに任された cgroup（subtree_control で memory・pids・cpu を有効にする）
//! <base>/supervisor/         デーモン自身（cgroups v2 では、子を持つ cgroup にプロセスを置けないため移す）
//! <base>/task-<id>/          タスクごとの cgroup（上限を書き、起動役が自分を移す）
//! ```

use otas_ports::{ResourceLimits, SandboxError};
use otas_types::TaskId;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WANTED: [&str; 3] = ["memory", "pids", "cpu"];

fn io(what: impl std::fmt::Display, e: std::io::Error) -> SandboxError {
    SandboxError::Unsupported(format!("{what}: {e}"))
}

/// タスクごとの cgroup を作り、上限を書き、終わったら片付ける。
#[derive(Debug)]
pub struct CgroupManager {
    base: PathBuf,
    controllers: BTreeSet<String>,
}

impl CgroupManager {
    /// 任された cgroup を開き、子の cgroup で memory・pids・cpu を使えるようにする。
    pub fn open(base: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let base = base.into();
        fs::create_dir_all(&base).map_err(|e| io(format!("{} を作れません", base.display()), e))?;

        // cgroups v2 では、子の cgroup で上限を使うために、プロセスを子の cgroup（葉）に移す必要がある
        let procs = fs::read_to_string(base.join("cgroup.procs"))
            .map_err(|e| io("cgroup.procs を読めません", e))?;
        if !procs.trim().is_empty() {
            let leaf = base.join("supervisor");
            fs::create_dir_all(&leaf).map_err(|e| io("supervisor の cgroup を作れません", e))?;
            for pid in procs.lines() {
                // 終わったプロセスは移せないので無視する
                let _ = fs::write(leaf.join("cgroup.procs"), pid);
            }
        }

        let available: BTreeSet<String> = fs::read_to_string(base.join("cgroup.controllers"))
            .map_err(|e| io("cgroup.controllers を読めません", e))?
            .split_whitespace()
            .map(String::from)
            .collect();
        let controllers: BTreeSet<String> = WANTED
            .iter()
            .filter(|c| available.contains(**c))
            .map(|c| c.to_string())
            .collect();
        let enable: Vec<String> = controllers.iter().map(|c| format!("+{c}")).collect();
        if !enable.is_empty() {
            fs::write(base.join("cgroup.subtree_control"), enable.join(" "))
                .map_err(|e| io("子の cgroup で上限を使えるようにできません", e))?;
        }
        Ok(Self { base, controllers })
    }

    /// 使える上限の種類（`memory`・`pids`・`cpu`）。
    pub fn controllers(&self) -> &BTreeSet<String> {
        &self.controllers
    }

    fn require(&self, controller: &str) -> Result<(), SandboxError> {
        if self.controllers.contains(controller) {
            Ok(())
        } else {
            Err(SandboxError::InvalidSpec(format!(
                "この環境の cgroups では {controller} の上限を使えません"
            )))
        }
    }

    /// タスクの cgroup を作り、上限を書く。
    pub fn create(&self, limits: &ResourceLimits) -> Result<PathBuf, SandboxError> {
        let dir = self.base.join(format!("task-{}", TaskId::new().0.simple()));
        fs::create_dir(&dir).map_err(|e| io("タスクの cgroup を作れません", e))?;
        let write = |file: &str, value: String| {
            fs::write(dir.join(file), value).map_err(|e| io(format!("{file} を書けません"), e))
        };
        let result = (|| {
            if let Some(bytes) = limits.memory_bytes {
                self.require("memory")?;
                write("memory.max", bytes.to_string())?;
                // スワップに逃がして上限をすり抜けないようにする（スワップがない環境では無視）
                let _ = fs::write(dir.join("memory.swap.max"), "0");
            }
            if let Some(n) = limits.processes {
                self.require("pids")?;
                write("pids.max", n.to_string())?;
            }
            if let Some(percent) = limits.cpu_percent {
                self.require("cpu")?;
                // 100000 マイクロ秒ごとに、割合の分だけ CPU を使える
                write(
                    "cpu.max",
                    format!("{} 100000", u64::from(percent.max(1)) * 1000),
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_dir(&dir);
            return Err(e);
        }
        Ok(dir)
    }

    /// タスクの cgroup を片付ける。残っているプロセスは止める。
    pub fn remove(&self, dir: &Path) {
        // cgroup.kill（Linux 5.14 以降）で、残っているプロセスをまとめて止める
        let _ = fs::write(dir.join("cgroup.kill"), "1");
        for _ in 0..100 {
            if fs::remove_dir(dir).is_ok() || !dir.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// 自分自身（起動役）を cgroup に移す。以後に作る子孫もすべてこの中で動く。
pub(crate) fn join(dir: &Path) -> Result<(), String> {
    fs::write(dir.join("cgroup.procs"), "0")
        .map_err(|e| format!("cgroup {} に入れません: {e}", dir.display()))
}
