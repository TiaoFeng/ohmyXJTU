//! 自訂義任務服務：把任務資料放在專屬執行緒上，與網路請求完全解耦。
//!
//! 任務原本和資料載入共用同一個工作執行緒。載入是一連串網路請求（一次
//! 往返可能數秒，最壞情況是 HTTP 逾時），控制任務只能在其中排空，因此按
//! 下 `^s` 之後可能要等好幾個往返才會被處理——介面看起來就像「卡在正在
//! 保存」，而且任務操作也會插進載入的步進之間，讓兩邊互相拖慢。
//!
//! 這裡把任務服務獨立出來：
//!
//! - 介面送出的 7 種任務操作（新增／修改／勾選／刪除…）由服務直接處理，
//!   只做本機檔案與密碼學運算，微秒級回應；網路再慢都與它無關。
//! - 其他任務（登入、資料載入、設定）原封不動轉給工作執行緒，順序不變。
//! - 解鎖、修改口令與會話停用由工作執行緒透過 [`TaskHandle`] 與服務協調；
//!   修改口令必須同步等待結果（兩個檔案要一起換），服務從不等待網路，
//!   因此這個等待很短。
//!
//! 介面看到的介面沒有改變：它仍然只面對一條 [`Job`] 通道與一條 [`Event`]
//! 通道，分流由本模組負責。

pub(crate) mod store;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use store::TaskStore;

use crate::credentials::Secret;
use crate::domain::todo::Task;
use crate::error::{AppError, AppResult};
use crate::task::protocol::{Event, Job, TaskReply};

/// 任務服務的控制代碼（工作者持有）。
///
/// 傳送端與介面用的是同一條通道：服務以先進先出處理，因此「解鎖時送出
/// `InitTasks`、介面在收到 `VaultReady` 之後才送任務操作」保證任務操作
/// 一定在載入完成之後才被處理。
#[derive(Debug, Clone)]
pub(crate) struct TaskHandle {
    jobs: Sender<Job>,
}

impl TaskHandle {
    /// 以既有的任務通道建立控制代碼。
    ///
    /// 介面用的是同一條通道：服務以先進先出處理，因此「解鎖時送出
    /// `InitTasks`、介面在收到 `VaultReady` 之後才送任務操作」保證任務操作
    /// 一定在載入完成之後才被處理。
    pub(crate) fn new(jobs: Sender<Job>) -> Self {
        Self { jobs }
    }

    /// 以口令載入任務檔（結果與提示由服務自行回報）。
    pub(crate) fn init(&self, passphrase: &Secret) {
        self.send(Job::InitTasks {
            passphrase: passphrase.clone(),
        });
    }

    /// 以新口令重新加密任務檔，並等待結果。
    ///
    /// 修改口令必須在寫入保險庫之前確認任務檔也已換鑰：否則兩個檔案會用
    /// 不同口令加密，下次解鎖時其中一個必定打不開。
    pub(crate) fn rekey(&self, passphrase: &Secret) -> AppResult<()> {
        let reply = TaskReply::new();
        if self
            .jobs
            .send(Job::RekeyTasks {
                passphrase: passphrase.clone(),
                reply: reply.clone(),
            })
            .is_err()
        {
            return Err(AppError::config("任务服务已停止"));
        }
        reply.wait().map_err(AppError::Crypto)
    }

    /// 丟棄記憶體中的任務金鑰（會話被停用時）。
    pub(crate) fn lock(&self) {
        self.send(Job::LockTasks);
    }

    /// 暫停任務寫入並等待服務確認（同步下載前）。
    ///
    /// 必須等到確認才開始下載：服務是獨立執行緒，未確認前仍可能處理排隊中的
    /// 任務操作並以舊清單落盤，覆寫剛下載的檔案。
    pub(crate) fn pause(&self) -> AppResult<()> {
        let reply = TaskReply::new();
        if self
            .jobs
            .send(Job::PauseTasks {
                reply: reply.clone(),
            })
            .is_err()
        {
            return Err(AppError::config("任务服务已停止"));
        }
        reply.wait().map_err(AppError::Crypto)
    }

    /// 恢復任務寫入（同步未實際下載本機檔案時）。
    pub(crate) fn resume(&self) {
        self.send(Job::ResumeTasks);
    }

    /// 任務服務已停止時靜默忽略：呼叫端無法在這個階段補救。
    fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

/// 只建立控制代碼而不啟動服務（測試用；送到服務的訊息會被丟棄）。
#[cfg(test)]
pub(crate) fn detached() -> TaskHandle {
    let (jobs, _orphan) = std::sync::mpsc::channel();
    TaskHandle { jobs }
}

/// 啟動任務服務執行緒。
///
/// `events` 與工作者共用（服務直接回報任務快照與提示）；`worker` 是服務轉送
/// 其餘任務的通道；`jobs` 是介面的任務來源（控制代碼與它共用同一條通道，
/// 見 [`TaskHandle::new`]）。
pub(crate) fn serve(
    events: Sender<Event>,
    worker: Sender<Job>,
    jobs: Receiver<Job>,
    path: PathBuf,
) -> AppResult<()> {
    let store = TaskStore::at(path);
    thread::Builder::new()
        .name("ohmyXJTU-tasks".to_owned())
        .spawn(move || run(store, events, worker, jobs))
        .map_err(|err| AppError::config(format!("无法启动任务服务线程：{err}")))?;
    Ok(())
}

/// 服務主迴圈：任務操作自行處理，其餘任務轉給工作執行緒。
fn run(mut store: TaskStore, events: Sender<Event>, worker: Sender<Job>, jobs: Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        match job {
            // 程式正在結束：轉給工作者（它負責收尾）後自己也停止。
            Job::Shutdown => {
                let _ = worker.send(Job::Shutdown);
                return;
            }
            Job::InitTasks { passphrase } => load(&mut store, &passphrase, &events),
            Job::RekeyTasks { passphrase, reply } => {
                let result = store.rekey(&passphrase).map_err(|err| err.to_string());
                reply.resolve(result);
            }
            Job::LockTasks => store.lock(),
            Job::PauseTasks { reply } => {
                store.pause();
                reply.resolve(Ok(()));
            }
            Job::ResumeTasks => store.resume(),
            job if job.is_task_op() => {
                if let Err(err) = operate(&mut store, job, &events) {
                    let what = "任务";
                    let _ = events.send(Event::Failed {
                        what: what.to_owned(),
                        message: err.to_string(),
                        target: crate::task::protocol::FailedTarget::Tasks,
                        site: None,
                        resource: None,
                    });
                }
            }
            // 其餘任務原封不動交給工作執行緒（工作者結束時一併停止）。
            job => {
                if worker.send(job).is_err() {
                    return;
                }
            }
        }
    }
}

/// 解鎖後載入任務檔並回報快照。
///
/// 任務檔讀取失敗不阻斷解鎖（登入與查詢都不依賴任務）；此時只提示並標記
/// 存儲不可用，後續的任務操作會回報同一個原因。
fn load(store: &mut TaskStore, passphrase: &Secret, events: &Sender<Event>) {
    match store.init(passphrase) {
        Ok(Some(notice)) => emit(events, Event::Warning(notice)),
        Ok(None) => {}
        Err(err) => {
            let message = format!("任务文件不可用（{err}）；本次会话无法保存自定义任务");
            store.mark_unavailable(message.clone());
            emit(events, Event::Warning(message));
        }
    }
    emit_snapshot(store, events);
}

/// 執行一次任務操作並回報快照與提示。
fn operate(store: &mut TaskStore, job: Job, events: &Sender<Event>) -> AppResult<()> {
    let notice = match job {
        Job::AddTask { task } => {
            store.add(task)?;
            "已添加任务".to_owned()
        }
        Job::UpdateTask { id, task } => {
            store.update(id, task)?;
            "任务已更新".to_owned()
        }
        Job::SetTaskDone { id, done } => {
            store.set_done(id, done)?;
            done_label(done)
        }
        Job::SetTasksDone { ids, done } => {
            let changed = store.set_done_many(&ids, done)?;
            if changed == 0 {
                "选中的任务状态没有变化".to_owned()
            } else if done {
                format!("已将 {changed} 个任务标记为完成")
            } else {
                format!("已将 {changed} 个任务标记为未完成")
            }
        }
        Job::DeleteTask { id } => {
            store.delete(id)?;
            "已删除任务".to_owned()
        }
        Job::DeleteTasks { ids } => {
            let deleted = store.delete_many(&ids)?;
            format!("已删除 {deleted} 个任务")
        }
        Job::DeleteCompletedTasks => {
            let deleted = store.delete_completed()?;
            if deleted == 0 {
                "没有已完成的任务".to_owned()
            } else {
                format!("已删除 {deleted} 个已完成任务")
            }
        }
        _ => return Ok(()),
    };
    emit_snapshot(store, events);
    emit(events, Event::Notice(notice));
    Ok(())
}

/// 完成狀態的提示文字。
fn done_label(done: bool) -> String {
    if done {
        "已标记完成"
    } else {
        "已标记未完成"
    }
    .to_owned()
}

/// 回報完整任務快照（介面據此更新清單）。
fn emit_snapshot(store: &TaskStore, events: &Sender<Event>) {
    let snapshot: Vec<Task> = store.snapshot();
    emit(events, Event::Tasks(snapshot));
}

/// 服務回報事件；介面已結束時忽略。
fn emit(events: &Sender<Event>, event: Event) {
    let _ = events.send(event);
}

#[cfg(test)]
#[path = "tests/store_test.rs"]
mod store_test;

#[cfg(test)]
#[path = "tests/service_test.rs"]
mod service_test;
