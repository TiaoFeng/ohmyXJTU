//! 背景任務：介面與網路查詢之間的橋樑。

pub mod protocol;
pub mod worker;

pub use protocol::{CoursesData, Event, FailedTarget, HomeworkIssue, HomeworkUpdate, Job};
pub use worker::spawn;
