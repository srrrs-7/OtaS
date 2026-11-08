//! Landlock によるファイルと TCP の制限。

use crate::plan::{LaunchPlan, PlanAccess};
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};

/// 必要とする Landlock の ABI（v4 で TCP の制限が加わった。Linux 6.7 以降）。
pub(crate) const REQUIRED_ABI: ABI = ABI::V4;

fn rights(access: PlanAccess, is_dir: bool) -> BitFlags<AccessFs> {
    let abi = REQUIRED_ABI;
    let rights = match access {
        PlanAccess::ReadOnly => AccessFs::ReadFile | AccessFs::ReadDir,
        PlanAccess::ReadExec => AccessFs::from_read(abi),
        PlanAccess::ReadWrite => AccessFs::from_all(abi) & !AccessFs::Execute,
    };
    if is_dir {
        rights
    } else {
        // ファイルに付けられるのは、ファイル向けの権利だけ
        rights & AccessFs::from_file(abi)
    }
}

/// 必要な ABI の Landlock が使えるか確かめる（自分には制限をかけない）。
pub(crate) fn probe() -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_ABI))
        .and_then(|r| r.handle_access(AccessNet::from_all(REQUIRED_ABI)))
        .and_then(|r| r.create())
        .map(|_| ())
        .map_err(|e| format!("Landlock ABI v4 を使えません: {e}"))
}

/// 自分自身に制限をかける。完全に適用できなければエラーにする（中途半端な状態では動かさない）。
pub(crate) fn restrict_self(plan: &LaunchPlan) -> Result<(), String> {
    let abi = REQUIRED_ABI;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?;
    if plan.deny_network {
        // 許可する TCP ポートの規則を1つも足さないので、bind と connect はすべて拒否される
        ruleset = ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|e| e.to_string())?;
    }
    let mut created = ruleset.create().map_err(|e| e.to_string())?;

    for rule in &plan.fs {
        let metadata = match std::fs::metadata(&rule.path) {
            Ok(m) => m,
            Err(_) if rule.optional => continue,
            Err(e) => return Err(format!("{} を開けません: {e}", rule.path.display())),
        };
        let fd = PathFd::new(&rule.path).map_err(|e| e.to_string())?;
        created = created
            .add_rule(PathBeneath::new(fd, rights(rule.access, metadata.is_dir())))
            .map_err(|e| format!("{} の規則を追加できません: {e}", rule.path.display()))?;
    }

    let status = created.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "Landlock を完全には適用できませんでした（{:?}）",
            status.ruleset
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_never_includes_execute() {
        assert!(!rights(PlanAccess::ReadWrite, true).contains(AccessFs::Execute));
        assert!(!rights(PlanAccess::ReadOnly, true).contains(AccessFs::Execute));
        assert!(rights(PlanAccess::ReadExec, true).contains(AccessFs::Execute));
    }

    #[test]
    fn file_rules_drop_directory_rights() {
        let r = rights(PlanAccess::ReadWrite, false);
        assert!(!r.contains(AccessFs::ReadDir));
        assert!(!r.contains(AccessFs::MakeDir));
        assert!(r.contains(AccessFs::WriteFile));
    }
}
