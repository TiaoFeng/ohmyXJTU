//! 堅果雲 WebDAV 同步：測試連線、導入、上傳／下載與設定。
//!
//! 同步屬網路任務，因此由工作者執行（任務服務明文不經網路）。磁碟上的檔案
//! 本來就是加密的，這裡只做檔案傳輸與三方比對，不在記憶體中解開密文。

use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::sync::config::{self, FileRecord, SyncConfig, SyncFile};
use crate::sync::engine::{self, Plan};
use crate::sync::webdav::WebDav;
use crate::task::protocol::{Event, SyncStateView};

use super::Worker;

/// 導入時暫存的設定與記錄：解鎖後才寫得進 `sync.vault`。
pub(super) struct PendingSync {
    config: SyncConfig,
    records: Vec<(SyncFile, FileRecord)>,
}

impl Worker {
    /// 建立 WebDAV 端點（依設定）。
    fn webdav_for(&self, config: &SyncConfig) -> WebDav {
        WebDav::new(
            Arc::clone(&self.webdav),
            config.url.clone(),
            &config.account,
            &config.app_password,
        )
    }

    /// 解鎖後載入同步設定，並（有導入暫存時）寫回設定與記錄。
    pub(super) fn init_sync(&mut self, passphrase: &str) {
        match self.sync.init(passphrase) {
            Ok(Some(notice)) => self.emit(Event::Warning(notice)),
            Ok(None) => {}
            Err(err) => {
                let message = format!("同步设置不可用（{err}）；本次会话无法使用坚果云同步");
                self.sync.mark_unavailable(message.clone());
                self.emit(Event::Warning(message));
            }
        }
        if let Some(pending) = self.pending_sync.take()
            && let Err(err) = self.persist_pending_sync(pending)
        {
            self.emit(Event::Warning(format!("同步设置保存失败：{err}")));
        }
        // 未設定同步時不必打擾介面（預設就是未設定）。
        if self.sync.config().is_some() {
            self.emit_sync_state();
        }
    }

    /// 把導入暫存的設定與記錄寫回 `sync.vault`。
    fn persist_pending_sync(&mut self, pending: PendingSync) -> AppResult<()> {
        for (file, record) in pending.records {
            self.sync.set_record(file, record)?;
        }
        self.sync.set_config(pending.config)
    }

    /// 回報目前同步設定（不含秘密）。
    fn emit_sync_state(&mut self) {
        let view = match self.sync.config() {
            Some(config) => SyncStateView {
                configured: true,
                url: config.url.clone(),
                account: config.account.clone(),
                auto_sync: config.auto_sync,
            },
            None => SyncStateView::default(),
        };
        self.emit(Event::SyncState(Box::new(view)));
    }

    /// 已設定時的連線設定（複製）；設定不可用時回錯。
    fn require_sync_config(&self) -> AppResult<SyncConfig> {
        let config = self
            .sync
            .config()
            .cloned()
            .ok_or_else(|| AppError::config("尚未配置坚果云同步"))?;
        config::validate(&config)?;
        Ok(config)
    }

    /// 測試連線與寫入權限（登入畫面與設定表單共用）。
    pub(super) fn sync_test_connection(&mut self, config: SyncConfig) -> AppResult<()> {
        let result = config::validate(&config).and_then(|()| self.webdav_for(&config).check());
        let event = match result {
            Ok(()) => Event::SyncTestResult {
                ok: true,
                message: "连接成功".to_owned(),
            },
            Err(err) => Event::SyncTestResult {
                ok: false,
                message: err.to_string(),
            },
        };
        self.emit(event);
        Ok(())
    }

    /// 從雲端導入：下載遠端檔案覆寫本機，暫存設定與記錄待解鎖後保存。
    pub(super) fn sync_import(&mut self, config: SyncConfig) -> AppResult<()> {
        config::validate(&config)?;
        let dav = self.webdav_for(&config);
        let records = engine::import(&dav, &self.sync_local)?;
        self.pending_sync = Some(PendingSync { config, records });
        self.emit(Event::SyncImported {
            message: "已从坚果云导入，请输入加密口令解锁".to_owned(),
        });
        Ok(())
    }

    /// 智慧同步：逐檔三方比對，只做無衝突的動作。
    pub(super) fn sync_now(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let (mut uploaded, mut downloaded) = (0_u32, 0_u32);
        let mut conflicts: Vec<&'static str> = Vec::new();
        for (file, path) in self.sync_local.clone() {
            let record = self.sync.record(file).clone();
            match engine::evaluate(&dav, file, &path, &record)? {
                Plan::Noop => {}
                Plan::Push => {
                    match engine::push(&dav, file, &path, record.etag.as_deref()) {
                        Ok(new) => {
                            self.sync.set_record(file, new)?;
                            uploaded += 1;
                        }
                        // 檢查後遠端被其他裝置改動（If-Match 未通過）：不覆蓋，改報衝突。
                        Err(AppError::WebDavConflict) => conflicts.push(file.remote_name()),
                        Err(err) => return Err(err),
                    }
                }
                Plan::Pull => {
                    if let Some(new) = engine::pull(&dav, file, &path)? {
                        self.sync.set_record(file, new)?;
                        downloaded += 1;
                    }
                }
                Plan::Conflict => conflicts.push(file.remote_name()),
            }
        }
        self.finish_sync(&conflicts, uploaded, downloaded);
        Ok(())
    }

    /// 自動同步（背景）：僅在「本機變更、遠端未變」時上傳。
    ///
    /// 其餘情況（無變更、遠端也變了、遠端較新）一律不動：自動模式絕不覆蓋
    /// 遠端，也不在本機狀態可能因此變舊時下載——需要下載或解決衝突時，留給
    /// 使用者在設定中按「立即同步」。背景失敗只留提示，不當成操作失敗。
    pub(super) fn sync_auto(&mut self) -> AppResult<()> {
        if !self.sync.config().is_some_and(|config| config.auto_sync) {
            return Ok(());
        }
        if let Err(err) = self.run_auto_upload() {
            self.emit(Event::Warning(format!("自动同步失败：{err}")));
        }
        Ok(())
    }

    /// 執行自動上傳（無變更時不動作）。
    fn run_auto_upload(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let mut uploaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            let record = self.sync.record(file).clone();
            if matches!(engine::evaluate(&dav, file, &path, &record)?, Plan::Push) {
                match engine::push(&dav, file, &path, record.etag.as_deref()) {
                    Ok(new) => {
                        self.sync.set_record(file, new)?;
                        uploaded += 1;
                    }
                    // 自動模式絕不覆蓋遠端：檢查後遠端被改動時靜默略過。
                    Err(AppError::WebDavConflict) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        if uploaded > 0 {
            self.emit(Event::SyncDone {
                summary: format!("已自动上传 {uploaded} 个文件"),
            });
        }
        Ok(())
    }

    /// 以本機覆蓋遠端（強制）。
    pub(super) fn sync_push(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let mut uploaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            if !path.is_file() {
                continue;
            }
            // 強制上傳：不帶條件請求，以本機覆蓋遠端。
            let new = engine::push(&dav, file, &path, None)?;
            self.sync.set_record(file, new)?;
            uploaded += 1;
        }
        self.emit(Event::SyncDone {
            summary: format!("已上传 {uploaded} 个文件"),
        });
        Ok(())
    }

    /// 以遠端覆蓋本機（強制）。
    pub(super) fn sync_pull(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let mut downloaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            if let Some(new) = engine::pull(&dav, file, &path)? {
                self.sync.set_record(file, new)?;
                downloaded += 1;
            }
        }
        self.emit(Event::SyncDone {
            summary: format!("已下载 {downloaded} 个文件"),
        });
        Ok(())
    }

    /// 儲存連線設定：先測試連線，通過才保存（「測試連線通過才啟用」）。
    pub(super) fn set_sync_config(&mut self, mut config: SyncConfig) -> AppResult<()> {
        config::validate(&config)?;
        self.webdav_for(&config).check()?;
        // 編輯既有設定時沿用目前的自動同步偏好，避免被重設為關閉。
        if let Some(existing) = self.sync.config() {
            config.auto_sync = existing.auto_sync;
        }
        self.sync.set_config(config)?;
        self.emit_sync_state();
        Ok(())
    }

    /// 切換自動同步。
    pub(super) fn set_sync_auto(&mut self, enabled: bool) -> AppResult<()> {
        self.sync.set_auto_sync(enabled)?;
        self.emit_sync_state();
        Ok(())
    }

    /// 清除同步設定與記錄。
    pub(super) fn clear_sync_config(&mut self) -> AppResult<()> {
        self.sync.clear()?;
        self.emit_sync_state();
        Ok(())
    }

    /// 回報同步結果或衝突。
    fn finish_sync(&mut self, conflicts: &[&'static str], uploaded: u32, downloaded: u32) {
        if conflicts.is_empty() {
            self.emit(Event::SyncDone {
                summary: format!("同步完成（上传 {uploaded}，下载 {downloaded}）"),
            });
        } else {
            self.emit(Event::SyncConflict {
                message: format!(
                    "以下文件在本机与云端都被修改，未自动覆盖：{}（请在设置中选择上传或下载）",
                    conflicts.join("、")
                ),
            });
        }
    }
}
