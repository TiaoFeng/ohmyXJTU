//! 任務服務測試：任務操作不與網路排隊、換口令同步結果與轉送其餘任務。

use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::*;
use crate::domain::todo::Priority;
use crate::error::AppError;

/// 測試用任務。
fn task(content: &str) -> Task {
    Task {
        id: 0,
        content: content.to_owned(),
        description: None,
        tag: None,
        deadline: None,
        priority: Priority::Low,
        completed: false,
    }
}

/// 任務服務的測試夾具：只啟動服務，不啟動網路工作者。
///
/// `worker` 是一條**沒有人讀取**的通道：用來證明任務操作不必等工作者；
/// 轉送過去的任務會躺在裡面（測試自己取出來檢查）。
struct Service {
    /// 介面用的任務送出端（與控制代碼同通道）。
    jobs: Sender<Job>,
    /// 服務回報的事件。
    events: std::sync::mpsc::Receiver<Event>,
    /// 服務轉送給工作者的任務（本測試不啟動工作者）。
    worker: std::sync::mpsc::Receiver<Job>,
    handle: TaskHandle,
    _dir: TempDir,
}

impl Service {
    fn new() -> Self {
        let dir = TempDir::new().expect("建立暂存目录");
        let (jobs, ui_rx) = channel();
        let (worker_tx, worker_rx) = channel();
        let (events_tx, events) = channel();
        serve(events_tx, worker_tx, ui_rx, dir.path().join("tasks.vault")).expect("启动任务服务");
        Self {
            jobs: jobs.clone(),
            events,
            worker: worker_rx,
            handle: TaskHandle::new(jobs),
            _dir: dir,
        }
    }

    /// 任務檔路徑。
    fn tasks_path(&self) -> std::path::PathBuf {
        self._dir.path().join("tasks.vault")
    }

    /// 以口令解鎖任務檔。
    fn init(&self, passphrase: &str) {
        self.handle.init(&passphrase.into());
    }

    /// 送出任務操作。
    fn send(&self, job: Job) {
        self.jobs.send(job).expect("送出任务");
    }

    /// 收集事件：先等最多 `first` 等到第一則事件，之後只收連續空檔之前的後續事件。
    ///
    /// 服務是獨立執行緒且解鎖要跑 Argon2id（測試建置下數百毫秒），因此不能
    /// 只靠固定等待。
    fn collect_events(&self, first: Duration, idle: Duration) -> Vec<Event> {
        let deadline = Instant::now() + first;
        let mut collected = Vec::new();
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(remaining) {
            Ok(event) => collected.push(event),
            Err(_) => return collected,
        }
        loop {
            match self.events.recv_timeout(idle) {
                Ok(event) => collected.push(event),
                Err(_) => return collected,
            }
        }
    }

    /// 送出任務操作並收集它回報的事件。
    fn dispatch(&self, job: Job) -> Vec<Event> {
        self.send(job);
        self.collect_events(Duration::from_secs(20), Duration::from_millis(150))
    }

    /// 最後一次回報的任務快照。
    fn snapshot(&self, events: &[Event]) -> Vec<Task> {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                Event::Tasks(tasks) => Some(tasks.clone()),
                _ => None,
            })
            .expect("应回报任务快照")
    }
}

#[test]
fn task_operations_do_not_wait_for_the_worker() {
    let service = Service::new();
    service.init("secret123");
    service.collect_events(Duration::from_secs(20), Duration::from_millis(150));

    // 工作者忙於網路請求（這裡刻意不讀取 worker 通道）：任務操作照樣立即完成。
    let started = Instant::now();
    let events = service.dispatch(Job::AddTask {
        task: task("写实验报告"),
    });
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "任务操作不应等待工作者，实际 {:?}",
        started.elapsed()
    );

    let tasks = service.snapshot(&events);
    assert_eq!(tasks.len(), 1, "任务应已写入");
    assert_eq!(tasks[0].content, "写实验报告");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(message) if message == "已添加任务")),
        "应回报提示：{events:?}"
    );
}

#[test]
fn other_jobs_are_forwarded_untouched() {
    let service = Service::new();
    service.send(Job::LoadSchedule { force: true });
    service.send(Job::Shutdown);

    // 轉送的順序不變；`Shutdown` 也會轉給工作者（服務自己同時停止）。
    assert!(matches!(
        service.worker.recv_timeout(Duration::from_secs(2)),
        Ok(Job::LoadSchedule { force: true })
    ));
    assert!(matches!(
        service.worker.recv_timeout(Duration::from_secs(2)),
        Ok(Job::Shutdown)
    ));
}

#[test]
fn rekey_reports_the_result_synchronously() {
    let service = Service::new();
    service.init("secret123");
    service.collect_events(Duration::from_secs(20), Duration::from_millis(150));
    service.dispatch(Job::AddTask { task: task("甲") });

    service
        .handle
        .rekey(&"new-passphrase".into())
        .expect("换口令");
    // 換口令後任務檔應能以新口令解開。
    let mut store = TaskStore::at(service.tasks_path());
    store.init("new-passphrase").expect("以新口令载入");
    assert_eq!(store.tasks().len(), 1);
}

#[test]
fn lock_drops_the_key_so_later_saves_fail() {
    let service = Service::new();
    service.init("secret123");
    service.collect_events(Duration::from_secs(20), Duration::from_millis(150));

    service.handle.lock();
    let events = service.dispatch(Job::AddTask { task: task("甲") });
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: crate::task::protocol::FailedTarget::Tasks,
                ..
            }
        )),
        "锁定后保存应回报任务失败：{events:?}"
    );
}

#[test]
fn unreadable_task_file_is_reported_without_blocking() {
    let service = Service::new();
    // 先以別的口令建立任務檔，再以另一個口令「解鎖」：內容無法解開。
    {
        let mut store = TaskStore::at(service.tasks_path());
        store.init("other-passphrase").expect("建立旧的任務檔");
        store.add(task("旧任务")).expect("写入");
    }
    let before = std::fs::read(service.tasks_path()).expect("读取旧档");

    service.init("secret123");
    let events = service.collect_events(Duration::from_secs(20), Duration::from_millis(150));
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message) if message.contains("任务文件无法读取")
        )),
        "应提示任务文件无法读取：{events:?}"
    );
    let tasks = service.snapshot(&events);
    assert!(tasks.is_empty(), "无法解开时不得凭空产生任务");
    assert_eq!(
        std::fs::read(service.tasks_path()).expect("原档仍在"),
        before,
        "原档必须保持原样（不得改名或覆盖）"
    );
}

#[test]
fn a_second_init_that_fails_does_not_let_later_saves_overwrite_the_file() {
    let service = Service::new();
    service.init("secret123");
    service.collect_events(Duration::from_secs(20), Duration::from_millis(150));
    service.dispatch(Job::AddTask {
        task: task("第一次加载"),
    });

    // 同一個服務實例再次初始化：原檔已被換成無法解讀的內容。
    std::fs::write(service.tasks_path(), b"corrupted by someone else").unwrap();
    service.init("secret123");
    let events = service.collect_events(Duration::from_secs(20), Duration::from_millis(150));
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message) if message.contains("任务文件无法读取")
        )),
        "再次初始化应提示任务文件无法读取：{events:?}"
    );

    // 後續保存必須失敗，且不得覆寫原檔（即使先前載入的金鑰還在記憶體中）。
    let events = service.dispatch(Job::AddTask {
        task: task("第二次"),
    });
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: crate::task::protocol::FailedTarget::Tasks,
                ..
            }
        )),
        "不可用时的保存应回报任务失败：{events:?}"
    );
    assert_eq!(
        std::fs::read(service.tasks_path()).unwrap(),
        b"corrupted by someone else",
        "原档必须保持原样"
    );
}

#[test]
fn failed_operation_keeps_the_target_and_message() {
    let service = Service::new();
    service.init("secret123");
    service.collect_events(Duration::from_secs(20), Duration::from_millis(150));

    let events = service.dispatch(Job::DeleteTask { id: 42 });
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: crate::task::protocol::FailedTarget::Tasks,
                message,
                ..
            } if message.contains("不存在") || message.contains("找不到")
        )),
        "找不到任务时应回报原因：{events:?}"
    );
    assert!(
        matches!(
            TaskStore::at(service.tasks_path()).init("secret123"),
            Ok(_) | Err(AppError::WrongPassphrase)
        ),
        "失败不应破坏任务档"
    );
}
