//! OtaS の共通語彙。
//!
//! すべてのクレートが使う型だけを置く。振る舞い（判定や実行）は持たない。
//! 依存は `serde` と `uuid` だけにして、どの OS でもどのクレートからでも使えるようにする。

mod action;
mod audit;
mod decision;
mod grant;
mod guarantees;
mod ids;
mod label;

pub use action::{AccessRequest, Action, Effect};
pub use audit::{AuditEntry, AuditRecord, Outcome};
pub use decision::Decision;
pub use grant::{AttenuationError, Grant, GrantSet};
pub use guarantees::{Guarantees, Level};
pub use ids::{ApprovalId, TaskId};
pub use label::{Label, LabelSet};
