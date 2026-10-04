//! 自訂義任務的加密存儲（`tasks.vault`）。
//!
//! 任務檔與憑證保險庫共用同一套信封格式與派生參數（見
//! [`crate::credentials::envelope`]），只有 AAD 前綴不同；金鑰自使用者的
//! 加密口令派生，磁碟上不含明文。
//!
//! 解鎖後派生的金鑰保留在記憶體中（[`Sealed`]），因此新增／修改／刪除任務
//! 不必重新輸入口令，也不必重跑昂貴的 Argon2id；每次保存仍使用新的 nonce。
//! 任務不隨帳號變動：換帳號只換站點憑證，任務檔的內容與金鑰都與帳號無關。
//!
//! 檔案無法讀取（損毀、或由其他口令建立）時不直接覆寫：先備份為
//! `tasks.vault.bak` 再以空清單重新開始，並回報提示訊息。

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::credentials::envelope::{self, Sealed};
use crate::domain::todo::{Task, sort_tasks};
use crate::error::{AppError, AppResult};
use crate::io;
use crate::task::protocol::Event;

use super::Worker;

/// 任務檔的 AAD 前綴（與憑證保險庫的 `ohmyXJTU-vault` 區隔）。
const AAD_PREFIX: &str = "ohmyXJTU-tasks";

/// 任務檔的內容。
#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskFile {
    /// 任務清單。
    #[serde(default)]
    tasks: Vec<Task>,
}

/// 任務存儲：記憶體中的任務清單＋加密的磁碟檔案。
pub(super) struct TaskStore {
    path: PathBuf,
    tasks: Vec<Task>,
    next_id: u64,
    sealed: Option<Sealed>,
    /// 本會話無法使用任務存儲時的原因（例如解鎖時讀檔失敗）。
    unavailable: Option<String>,
}

impl TaskStore {
    /// 使用指定路徑（便於測試）。
    pub(super) fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            tasks: Vec::new(),
            next_id: 0,
            sealed: None,
            unavailable: None,
        }
    }

    /// 使用標準資料目錄下的任務檔。
    pub(super) fn at_default_path() -> AppResult<Self> {
        Ok(Self::at(io::tasks_path()?))
    }

    /// 目前任務（已排序；測試用）。
    #[cfg(test)]
    pub(super) fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// 任務快照（排序後）。
    pub(super) fn snapshot(&self) -> Vec<Task> {
        self.tasks.clone()
    }

    /// 以口令載入任務檔並記住金鑰。
    ///
    /// 檔案不存在時只準備好新信封（首次保存才落盤），避免從未使用任務的
    /// 使用者多出一個檔案。回傳需要告知使用者的提示（原檔無法讀取而備份、
    /// 權限過寬等）；檔案層級的讀取失敗（非「不存在」）仍向上傳播。
    pub(super) fn init(&mut self, passphrase: &str) -> AppResult<Option<String>> {
        let mut notices: Vec<String> = Vec::new();
        self.unavailable = None;
        match io::read_private(&self.path) {
            Ok(bytes) => {
                if self.open(passphrase, &bytes).is_err() {
                    let backup = self.backup_path();
                    let _ = std::fs::rename(&self.path, &backup);
                    self.tasks.clear();
                    self.next_id = 0;
                    self.sealed = None;
                    self.provision(passphrase)?;
                    notices.push(format!(
                        "任务文件无法读取，已备份为 {} 并清空",
                        backup.display()
                    ));
                }
            }
            Err(AppError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                self.provision(passphrase)?;
            }
            Err(err) => return Err(err),
        }
        if let Ok(true) = io::ensure_private(&self.path) {
            notices.push(format!(
                "任务文件权限过宽（其他用户可读），已收紧为仅本人可读写：{}",
                self.path.display()
            ));
        }
        Ok(if notices.is_empty() {
            None
        } else {
            Some(notices.join("；"))
        })
    }

    /// 新增任務：識別碼由存儲指派。
    pub(super) fn add(&mut self, mut task: Task) -> AppResult<()> {
        self.apply(move |tasks, next_id| {
            task.id = *next_id;
            *next_id = next_id.saturating_add(1);
            tasks.push(task);
            Ok(())
        })
    }

    /// 以新內容覆蓋指定任務（識別碼由參數決定，不受表單內容影響）。
    pub(super) fn update(&mut self, id: u64, mut task: Task) -> AppResult<()> {
        self.apply(move |tasks, _| {
            let target = tasks
                .iter_mut()
                .find(|existing| existing.id == id)
                .ok_or(AppError::TaskNotFound)?;
            task.id = id;
            *target = task;
            Ok(())
        })
    }

    /// 設定單一任務的完成狀態。
    pub(super) fn set_done(&mut self, id: u64, done: bool) -> AppResult<()> {
        self.apply(move |tasks, _| {
            let target = tasks
                .iter_mut()
                .find(|existing| existing.id == id)
                .ok_or(AppError::TaskNotFound)?;
            target.completed = done;
            Ok(())
        })
    }

    /// 設定多個任務的完成狀態；回傳實際變更的數量。
    pub(super) fn set_done_many(&mut self, ids: &[u64], done: bool) -> AppResult<usize> {
        let ids: HashSet<u64> = ids.iter().copied().collect();
        self.apply(move |tasks, _| {
            let mut changed = 0;
            for task in tasks.iter_mut().filter(|task| ids.contains(&task.id)) {
                if task.completed != done {
                    task.completed = done;
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }

    /// 刪除單一任務。
    pub(super) fn delete(&mut self, id: u64) -> AppResult<()> {
        self.apply(move |tasks, _| {
            let before = tasks.len();
            tasks.retain(|task| task.id != id);
            if tasks.len() == before {
                return Err(AppError::TaskNotFound);
            }
            Ok(())
        })
    }

    /// 刪除多個任務；回傳刪除數量。
    pub(super) fn delete_many(&mut self, ids: &[u64]) -> AppResult<usize> {
        let ids: HashSet<u64> = ids.iter().copied().collect();
        self.apply(move |tasks, _| {
            let before = tasks.len();
            tasks.retain(|task| !ids.contains(&task.id));
            Ok(before - tasks.len())
        })
    }

    /// 刪除所有已完成任務；回傳刪除數量。
    pub(super) fn delete_completed(&mut self) -> AppResult<usize> {
        self.apply(|tasks, _| {
            let before = tasks.len();
            tasks.retain(|task| !task.completed);
            Ok(before - tasks.len())
        })
    }

    /// 換口令：以新口令重新加密整個任務檔（新鹽值與金鑰）。
    ///
    /// 寫入成功後才替換記憶體中的信封；寫入失敗時金鑰與磁碟內容都維持原狀。
    /// 呼叫端負責與憑證保險庫之間的順序一致（見 `credentials::change_passphrase`）。
    pub(super) fn rekey(&mut self, passphrase: &str) -> AppResult<()> {
        let plaintext = self.serialize()?;
        let (bytes, sealed) = envelope::seal(passphrase, AAD_PREFIX, &plaintext)?;
        io::write_private_atomic(&self.path, &bytes)?;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 標記本會話無法使用任務存儲（解鎖時讀檔失敗）；之後的操作會回報此原因。
    pub(super) fn mark_unavailable(&mut self, message: String) {
        self.unavailable = Some(message);
    }

    /// 丟棄金鑰（工作階段停用時）：任務內容仍保留在記憶體，但無法再寫入。
    pub(super) fn lock(&mut self) {
        self.sealed = None;
    }

    /// 任務檔路徑。
    #[cfg(test)]
    pub(super) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 解開並載入任務檔內容。
    fn open(&mut self, passphrase: &str, bytes: &[u8]) -> AppResult<()> {
        let (plaintext, sealed) = envelope::unseal(passphrase, AAD_PREFIX, bytes)?;
        let file: TaskFile = serde_json::from_slice(&plaintext).map_err(|err| {
            AppError::VaultFile(format!(
                "任务内容无法解析（{}）",
                crate::error::describe_json_failure(err.classify())
            ))
        })?;
        self.tasks = file.tasks;
        sort_tasks(&mut self.tasks);
        self.next_id = self
            .tasks
            .iter()
            .map(|task| task.id)
            .max()
            .map_or(0, |id| id.saturating_add(1));
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 準備全新信封（不寫檔）。
    fn provision(&mut self, passphrase: &str) -> AppResult<()> {
        let plaintext = self.serialize()?;
        let (_, sealed) = envelope::seal(passphrase, AAD_PREFIX, &plaintext)?;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 序列化目前任務。
    fn serialize(&self) -> AppResult<Vec<u8>> {
        serde_json::to_vec(&TaskFile {
            tasks: self.tasks.clone(),
        })
        .map_err(|err| AppError::Crypto(format!("任务序列化失败：{err}")))
    }

    /// 以目前信封寫入任務檔（沿用同一鹽值，只換 nonce）。
    fn save(&self) -> AppResult<()> {
        let sealed = match (&self.sealed, &self.unavailable) {
            (Some(sealed), _) => sealed,
            (None, Some(message)) => return Err(AppError::Crypto(message.clone())),
            (None, None) => {
                return Err(AppError::Crypto("任务存储尚未解锁".to_owned()));
            }
        };
        let bytes = envelope::reseal(sealed, AAD_PREFIX, &self.serialize()?)?;
        io::write_private_atomic(&self.path, &bytes)
    }

    /// 套用一次異動並寫入；任何一步失敗都還原記憶體狀態（磁碟不變）。
    fn apply<T>(
        &mut self,
        change: impl FnOnce(&mut Vec<Task>, &mut u64) -> AppResult<T>,
    ) -> AppResult<T> {
        let tasks_backup = self.tasks.clone();
        let next_backup = self.next_id;
        let outcome = change(&mut self.tasks, &mut self.next_id).and_then(|value| {
            sort_tasks(&mut self.tasks);
            self.save().map(|()| value)
        });
        if outcome.is_err() {
            self.tasks = tasks_backup;
            self.next_id = next_backup;
        }
        outcome
    }

    /// 無法讀取時原檔的去向。
    fn backup_path(&self) -> PathBuf {
        let mut path = self.path.clone().into_os_string();
        path.push(".bak");
        PathBuf::from(path)
    }
}

/// 背景執行緒端的任務操作。
///
/// 這些都是控制任務：任務只涉及本地檔案，不應排在長查詢之後，也不受
/// 登入流程影響（登入互動期間仍可新增／勾選任務）。
impl Worker {
    /// 解鎖後載入任務檔並回報快照。
    ///
    /// 任務檔讀取失敗不阻斷解鎖（登入與查詢都不依賴任務）；此時只提示並
    /// 標記存儲不可用，後續的任務操作會回報同一個原因。
    pub(super) fn init_tasks(&mut self, passphrase: &str) {
        match self.tasks.init(passphrase) {
            Ok(Some(notice)) => self.emit(Event::Notice(notice)),
            Ok(None) => {}
            Err(err) => {
                let message = format!("任务文件不可用（{err}）；本次会话无法保存自定义任务");
                self.tasks.mark_unavailable(message.clone());
                self.emit(Event::Notice(message));
            }
        }
        self.emit_tasks();
    }

    /// 回報任務快照（完整清單）。
    pub(super) fn emit_tasks(&mut self) {
        let snapshot = self.tasks.snapshot();
        self.emit(Event::Tasks(snapshot));
    }

    /// 新增任務。
    pub(super) fn add_task(&mut self, task: Task) -> AppResult<()> {
        self.tasks.add(task)?;
        self.emit_tasks();
        self.emit(Event::Notice("已添加任务".to_owned()));
        Ok(())
    }

    /// 以新內容覆蓋任務。
    pub(super) fn update_task(&mut self, id: u64, task: Task) -> AppResult<()> {
        self.tasks.update(id, task)?;
        self.emit_tasks();
        self.emit(Event::Notice("任务已更新".to_owned()));
        Ok(())
    }

    /// 設定單一任務的完成狀態。
    pub(super) fn set_task_done(&mut self, id: u64, done: bool) -> AppResult<()> {
        self.tasks.set_done(id, done)?;
        self.emit_tasks();
        self.emit(Event::Notice(
            if done {
                "已标记完成"
            } else {
                "已标记未完成"
            }
            .to_owned(),
        ));
        Ok(())
    }

    /// 批次設定完成狀態（多選）。
    pub(super) fn set_tasks_done(&mut self, ids: &[u64], done: bool) -> AppResult<()> {
        let changed = self.tasks.set_done_many(ids, done)?;
        self.emit_tasks();
        let message = if changed == 0 {
            "选中的任务状态没有变化".to_owned()
        } else if done {
            format!("已将 {changed} 个任务标记为完成")
        } else {
            format!("已将 {changed} 个任务标记为未完成")
        };
        self.emit(Event::Notice(message));
        Ok(())
    }

    /// 刪除單一任務。
    pub(super) fn delete_task(&mut self, id: u64) -> AppResult<()> {
        self.tasks.delete(id)?;
        self.emit_tasks();
        self.emit(Event::Notice("已删除任务".to_owned()));
        Ok(())
    }

    /// 批次刪除任務（多選）。
    pub(super) fn delete_tasks(&mut self, ids: &[u64]) -> AppResult<()> {
        let deleted = self.tasks.delete_many(ids)?;
        self.emit_tasks();
        self.emit(Event::Notice(format!("已删除 {deleted} 个任务")));
        Ok(())
    }

    /// 刪除所有已完成任務。
    pub(super) fn delete_completed_tasks(&mut self) -> AppResult<()> {
        let deleted = self.tasks.delete_completed()?;
        self.emit_tasks();
        let message = if deleted == 0 {
            "没有已完成的任务".to_owned()
        } else {
            format!("已删除 {deleted} 个已完成任务")
        };
        self.emit(Event::Notice(message));
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/tasks_test.rs"]
mod tasks_test;
