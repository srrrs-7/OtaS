use serde::{Deserialize, Serialize};

/// 保証の強さ。`Absent < Weak < Strong` の順に強い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// 保証なし。
    Absent,
    /// 部分的な保証（迂回の余地がある）。
    Weak,
    /// カーネルなどで強制される保証。
    Strong,
}

/// 実行環境（OS アダプタ）が自己申告する保証。
///
/// 抽象化しても保証の差は隠さない。ポリシーはこれを見て、環境が弱ければ操作を拒否できる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guarantees {
    /// ファイルの隔離（見えるもの・触れるものの制限）。
    pub fs_isolation: Level,
    /// ネットワークの隔離（通信先の制限）。
    pub net_isolation: Level,
    /// システムコールの制限。
    pub syscall_filter: Level,
    /// 取り消しのためのスナップショット。
    pub snapshot: Level,
}

impl Guarantees {
    /// 何も保証しない環境。
    pub const NONE: Guarantees = Guarantees {
        fs_isolation: Level::Absent,
        net_isolation: Level::Absent,
        syscall_filter: Level::Absent,
        snapshot: Level::Absent,
    };

    /// すべてを強く保証する環境（Linux 本体を想定）。
    pub const STRONG: Guarantees = Guarantees {
        fs_isolation: Level::Strong,
        net_isolation: Level::Strong,
        syscall_filter: Level::Strong,
        snapshot: Level::Strong,
    };

    /// すべての項目で `min` 以上の保証があるか。
    pub fn satisfies(&self, min: &Guarantees) -> bool {
        self.fs_isolation >= min.fs_isolation
            && self.net_isolation >= min.net_isolation
            && self.syscall_filter >= min.syscall_filter
            && self.snapshot >= min.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfies_is_per_field() {
        let mut env = Guarantees::STRONG;
        assert!(env.satisfies(&Guarantees::STRONG));
        env.net_isolation = Level::Weak;
        assert!(!env.satisfies(&Guarantees::STRONG));
        assert!(env.satisfies(&Guarantees::NONE));
    }
}
