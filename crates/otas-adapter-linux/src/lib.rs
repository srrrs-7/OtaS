//! OtaS の Linux アダプタ。
//!
//! # サンドボックスの仕組み
//!
//! 親（`otasd` など）は [`LinuxSandbox::spawn`] で起動役のバイナリ `otas-sandbox-exec` を起動する。
//! 起動役は自分自身に次の制限をかけてから、対象のプログラムを `exec` する。
//! 制限は `exec` 後も引き継がれ、外すことはできない。
//!
//! 1. 親が死んだら道連れにする（`PR_SET_PDEATHSIG`）
//! 2. 資源の上限（`setrlimit`。コアダンプは常に無効）
//! 3. Landlock：許可したパス以外のファイルに触れない。TCP の bind と connect をすべて拒否する
//! 4. seccomp：危険なシステムコールを拒否する（名前空間の作成、ptrace、カーネルモジュール、
//!    io_uring、ソケットの作成など）
//! 5. 環境変数を空にし、指定したものだけを渡して `exec`
//!
//! どれも特権を必要としないので、権限をすべて外したコンテナの中でも使える。
//!
//! # 名前空間を使う方式（[`LinuxSandbox::detect_namespaced`]）
//!
//! デーモンが root で動く場合は、上の制限の前に名前空間を作り、許可したものだけを置いた
//! 新しいルートに移ってから一般ユーザーに切り替える（`namespace` モジュール）。
//! 許可していないパスはサンドボックスの中に存在せず、作業領域は `/data` として見える。
//! OverlayFS の作業領域（書いたときだけコピー）もこの方式で使える。
//!
//! # 保証の申告
//!
//! - ファイルの隔離：Strong（Landlock がカーネルで強制する）
//! - ネットワークの隔離：Strong（Landlock の TCP 制限と、seccomp によるソケット作成の拒否）
//! - システムコールの制限：Weak（危険なものを列挙して拒否する方式。許可リスト方式ではない）
//! - スナップショット：Absent（作業領域のアダプタは未実装）
//!
//! Landlock ABI v4（Linux 6.7 以降）が必要。使えない環境では、起動を拒否する（制限なしでは動かさない）。

#![cfg(target_os = "linux")]

mod cgroup;
mod landlock_rules;
mod launch;
mod namespace;
mod plan;
mod sandbox;
mod seccomp_filter;

pub use cgroup::CgroupManager;
pub use launch::{exec_sandboxed, run_pid1};
pub use plan::{
    LaunchPlan, NamespacePlan, PlanAccess, PlanFsRule, PlanLimits, PlanMount, PlanMountSource,
};
pub use sandbox::{LAUNCHER_FAILURE_EXIT_CODE, LinuxSandbox, NamespaceConfig};
