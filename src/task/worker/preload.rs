//! 解鎖後的背景預熱：先登入兩個站點，再預載四個頁面的資料。
//!
//! 這個模組只負責**安排順序**：登入沿用 [`Worker::begin_login`] 與既有的
//! 互動覆蓋層（圖片驗證碼、簡訊驗證），載入沿用既有的資料任務。預載失敗
//! 不影響解鎖——四個頁面維持未載入，進入該頁時照常惰性載入並重試登入。

use crate::error::{AppError, AppResult};
use crate::session::SiteKind;
use crate::task::protocol::Job;

use super::Worker;

/// 預載要登入的站點，順序即登入順序。
///
/// 考勤系統先登入：課表是最常看的第一頁，而兩個站點的登入流程幾乎相同，
/// 先做完考勤能讓第一個頁面更快可用。
const PRELOAD_SITES: [SiteKind; 2] = [SiteKind::Attendance, SiteKind::Lms];

impl Worker {
    /// 預載一步：登入下一個尚未登入的站點，或（兩個站點都就緒時）排入四個頁面的載入。
    ///
    /// 這是一個可重入的狀態機：`begin_login` 需要使用者輸入驗證碼時會把流程
    /// 留在 `flow`，登入完成後由 [`Worker::finish_login`] 以同一個
    /// [`Job::Preload`] 重新進入本函式，接著處理下一個站點。站點是否已登入
    /// 直接問會話管理器。
    pub(super) fn preload(&mut self) -> AppResult<()> {
        // 已經有登入在進行（例如使用者解鎖後立刻按 `r`，資料任務先觸發了自動
        // 重登）：不重啟它——那會丟掉目前流程等待續跑的驗證碼輸入——改為記下
        // 「預載還沒做」，由這次登入的收尾補做（見 [`Worker::finish_login`]）。
        //
        // 只有預載自己發起的登入帶著 [`Job::Preload`] 續跑；其他任務發起的
        // 登入收尾時只續跑它自己的任務，不在這裡記下就會整批遺失。
        if self.flow.is_some() {
            self.preload_pending = true;
            return Ok(());
        }
        match self.pending_preload_site()? {
            Some(site) => self.begin_login(site, Some(Job::Preload)),
            None => {
                self.queue_preload_jobs();
                Ok(())
            }
        }
    }

    /// 尚未登入、且屬於預載範圍的站點。
    fn pending_preload_site(&self) -> AppResult<Option<SiteKind>> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| AppError::config("会话尚未建立，请先解锁凭证"))?;
        Ok(PRELOAD_SITES
            .into_iter()
            .find(|site| !session.is_logged_in(*site)))
    }

    /// 排入四個頁面的載入任務。
    ///
    /// 順序刻意由便宜到貴：課表與考勤流水各只要一兩個請求，思源學堂的課程
    /// 清單也很輕，而作業彙總要逐門課程查活動與提交狀態，可能花上十秒。
    /// 工作者同一時間只跑一個資料任務，把作業放在最後，使用者切換其他頁面時
    /// 才不會排在它後面乾等。
    fn queue_preload_jobs(&mut self) {
        // 先收下介面已經送出的資料請求（解鎖時當前頁面的載入，或登入期間
        // 使用者按下的 `r`）：它們與預載排入的任務同鍵，必須在同一份佇列裡
        // 一起參與去重，否則同一個頁面會被查詢兩次——第二次的結果會把使用者
        // 已經移動過的選取重設回第一項。
        if !self.drain_channel(&Job::Preload) {
            // 收到結束指令：程式正在退出，不必再排入預載。
            return;
        }
        for job in [
            Job::LoadSchedule { force: false },
            Job::LoadFlow { page: 1 },
            Job::LoadCourses { force: false },
            Job::LoadHomework { force: false },
        ] {
            self.merge_data_job(job, None);
        }
    }
}
