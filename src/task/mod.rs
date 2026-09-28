//! 背景任務：介面與網路查詢之間的橋樑。

pub mod worker;

pub use worker::{CoursesData, Event, FailedTarget, HomeworkIssue, HomeworkUpdate, Job, spawn};
