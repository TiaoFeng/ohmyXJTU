//! 作業載入的分階段計時（診斷用）。
//!
//! 目的只是回答「時間花在哪裡」：冷啟動、五分鐘內再次進入與按 `r` 刷新時，
//! 分別有多少時間花在自動重新登入、考勤當前學期、課程清單、每門課程的活動
//! 列表、活動詳情與提交記錄，以及第一項作業多久才出現。
//!
//! 設定環境變數 `OHMYXJTU_TIMING=1` 時，作業載入完成後以單行訊息輸出一次；
//! 未設定時只做計數與取樣（成本是幾個 `Instant::now()`），不輸出任何事件，
//! 行為與訊息完全不變。
//!
//! 只收集類別與數字（不含網址、憑證或任何回應內容），且僅存在記憶體。

use std::time::{Duration, Instant};

/// 環境變數：設為 `1` 時輸出分階段計時。
const TIMING_ENV: &str = "OHMYXJTU_TIMING";

/// 是否啟用分階段計時。
pub(super) fn enabled() -> bool {
    matches!(std::env::var(TIMING_ENV).as_deref(), Ok("1"))
}

/// 計時階段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// 自動重新登入：`begin_login` 到流程結束的牆鐘時間（含等待使用者輸入
    /// 驗證碼或簡訊的時間）。
    Login,
    /// 考勤當前學期。
    Term,
    /// 課程清單。
    Courses,
    /// 每門課程的活動列表。
    Activities,
    /// 作業的活動詳情。
    Detail,
    /// 作業的提交記錄（詳情已取得後的部分）。
    Submission,
}

/// 單一階段的累計。
#[derive(Debug, Default, Clone, Copy)]
struct Stage {
    /// 累計耗時。
    elapsed: Duration,
    /// 處理事項數（含快取命中，用於分辨「查得多」與「查得慢」）。
    items: usize,
}

/// 一次作業載入的分階段計時。
#[derive(Debug, Default)]
pub(super) struct LoadTiming {
    /// 是否於結束時輸出報告。
    enabled: bool,
    /// 計時起點；`None` 代表目前沒有進行中的計時。
    start: Option<Instant>,
    login: Stage,
    term: Stage,
    courses: Stage,
    activities: Stage,
    detail: Stage,
    submission: Stage,
    /// 第一項作業出現的時間（自載入開始算起）。
    first_item: Option<Duration>,
    /// 第一項作業被送進界面的時間（自載入開始算起）。
    first_shown: Option<Duration>,
    /// 快取命中次數。
    hits: usize,
    /// 本學期作業項數。
    homework_items: usize,
    /// 活動列表中帶 `submit_by_group`／`user_submit_count`／`group_id` 的作業數。
    ///
    /// 用來回答「能不能靠列表就判定提交狀態、省下每項一次詳情請求」：
    /// 若三欄几乎都有值，詳情就只需在開啟說明時才抓。
    list_submit_by_group: usize,
    list_submit_count: usize,
    list_group_id: usize,
}

impl LoadTiming {
    /// 目前是否沒有進行中的計時。
    pub(super) fn is_idle(&self) -> bool {
        self.start.is_none()
    }

    /// 開始一次新的計時（覆寫先前的狀態）。
    pub(super) fn begin(&mut self, enabled: bool) {
        *self = Self {
            enabled,
            start: Some(Instant::now()),
            ..Self::default()
        };
    }

    /// 放棄目前的計時：載入被取消或已由新的請求接手，不輸出報告。
    pub(super) fn abandon(&mut self) {
        *self = Self::default();
    }

    /// 取樣起點（階段開始前呼叫）。
    pub(super) fn mark(&self) -> Instant {
        Instant::now()
    }

    /// 累計一個階段；`items` 為該階段處理的項目數。
    ///
    /// 沒有進行中的計時時仍會累計，但下次 [`Self::begin`] 會覆寫，因此不會
    /// 影響任何一次真正的載入。
    pub(super) fn record(&mut self, phase: Phase, started: Instant, items: usize) {
        let elapsed = started.elapsed();
        let stage = match phase {
            Phase::Login => &mut self.login,
            Phase::Term => &mut self.term,
            Phase::Courses => &mut self.courses,
            Phase::Activities => &mut self.activities,
            Phase::Detail => &mut self.detail,
            Phase::Submission => &mut self.submission,
        };
        stage.elapsed += elapsed;
        stage.items += items;
    }

    /// 記下第一項作業出現的時間（只記第一次）。
    pub(super) fn note_first_item(&mut self) {
        if let Some(start) = self.start
            && self.first_item.is_none()
        {
            self.first_item = Some(start.elapsed());
        }
    }

    /// 記下第一次把作業送進界面的時間（只記第一次）。
    pub(super) fn note_first_shown(&mut self) {
        if let Some(start) = self.start
            && self.first_shown.is_none()
        {
            self.first_shown = Some(start.elapsed());
        }
    }

    /// 記錄一項作業在活動列表中的欄位齊全度（診斷用）。
    pub(super) fn note_list_item(
        &mut self,
        submit_by_group: bool,
        submit_count: bool,
        group_id: bool,
    ) {
        self.homework_items += 1;
        self.list_submit_by_group += usize::from(submit_by_group);
        self.list_submit_count += usize::from(submit_count);
        self.list_group_id += usize::from(group_id);
    }

    /// 記錄一次快取命中。
    pub(super) fn note_hit(&mut self) {
        self.hits += 1;
    }

    /// 結束計時：啟用時回傳單行報告，否則回傳 `None`。
    pub(super) fn take_report(&mut self) -> Option<String> {
        let start = self.start.take()?;
        let total = start.elapsed();
        let report = self.enabled.then(|| self.format(total));
        *self = Self::default();
        report
    }

    /// 單行報告。
    ///
    /// `总` 為牆鐘時間：自使用者送出請求到本次載入結束，含自動重新登入與
    /// 等待輸入驗證碼／簡訊的時間，因此「登录」是它的子集合，其餘階段是
    /// 扣除登入後的實際資料查詢時間。
    fn format(&self, total: Duration) -> String {
        let secs = |duration: Duration| duration.as_secs_f64();
        let stamp = |mark: Option<Duration>| {
            mark.map_or_else(
                || "-".to_owned(),
                |elapsed| format!("{:.1}s", secs(elapsed)),
            )
        };
        format!(
            "计时 总{:.1}s 登录{:.1}s 学期{:.1}s 课程{:.1}s 活动{:.1}s/{} 详情{:.1}s 提交{:.1}s 首项{} 首显{} 命中{} 作业{}[组{} 数{} ID{}]",
            secs(total),
            secs(self.login.elapsed),
            secs(self.term.elapsed),
            secs(self.courses.elapsed),
            secs(self.activities.elapsed),
            self.activities.items,
            secs(self.detail.elapsed),
            secs(self.submission.elapsed),
            stamp(self.first_item),
            stamp(self.first_shown),
            self.hits,
            self.homework_items,
            self.list_submit_by_group,
            self.list_submit_count,
            self.list_group_id,
        )
    }
}

#[cfg(test)]
#[path = "tests/timing_test.rs"]
mod timing_test;
