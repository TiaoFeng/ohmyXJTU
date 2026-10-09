//! 背景任務：介面與網路查詢之間的橋樑。

pub mod protocol;
/// 自訂義任務服務（獨立執行緒；任務操作不經網路）。
pub(crate) mod tasks;
pub mod worker;

pub use protocol::{
    CoursesData, Event, FailedTarget, HomeworkIssue, HomeworkUpdate, Job, SyncStateView,
};
pub use worker::spawn;
