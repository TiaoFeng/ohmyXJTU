//! 同步引擎：三方比對與上傳／下載。
//!
//! 只做檔案層面的操作，不碰介面或通道；由工作者（見 `crate::task::worker`）呼叫。
//! 「三方比對」指的是本機現況、遠端現況與上次同步的記錄：據此判斷只有一邊變更
//!（安全地單向上傳或下載），或兩邊都變更（衝突，不自動覆蓋）。

use std::path::{Path, PathBuf};

use crate::credentials::envelope;
use crate::error::{AppError, AppResult};
use crate::io;
use crate::sync::config::{FileRecord, SyncFile};
use crate::sync::webdav::{Precondition, RemoteMeta, WebDav};

/// 檔案內容的指紋（用於偵測本機是否變更）。
///
/// FNV-1a 64 位元：非密碼學雜湊，只求「內容改變時指紋幾乎必然改變」，且在版本
/// 之間保持穩定（`std::hash` 不保證跨版本一致，不能拿來持久化）。
pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 依「本機是否變更」「遠端是否變更」決定的動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 兩邊一致，無需動作。
    Noop,
    /// 只有本機變更 → 上傳。
    Push,
    /// 只有遠端變更 → 下載。
    Pull,
    /// 兩邊都變更 → 衝突，不自動覆蓋。
    Conflict,
}

/// 由變更狀態決定計畫（純函式）。
pub(crate) fn plan(local_changed: bool, remote_changed: bool) -> Plan {
    match (local_changed, remote_changed) {
        (false, false) => Plan::Noop,
        (true, false) => Plan::Push,
        (false, true) => Plan::Pull,
        (true, true) => Plan::Conflict,
    }
}

/// 單一檔案的評估結果：計畫，以及遠端是否確實不存在。
///
/// 「遠端不存在」需要獨立回報，因為上傳的前置條件取決於它：遠端**確認不存在**
/// 時應帶 `If-None-Match: *`（避免與其他裝置的首次建立互撞），而「遠端存在但
/// 沒有 `ETag`」只能無條件覆寫。這個區別在只看 [`Plan::Push`] 時會遺失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evaluation {
    /// 三方比對後的計畫。
    pub plan: Plan,
    /// 遠端是否確實不存在（`HEAD` 回 `404`／`409`／`410`）。
    pub remote_absent: bool,
}

impl Evaluation {
    /// 上傳時要帶的前置條件：遠端確認不存在時要求「必須不存在」，否則沿用
    /// 上次記錄的 `ETag`（無 `ETag` 則無條件覆寫）。
    pub(crate) fn precondition<'a>(&self, etag: Option<&'a str>) -> Precondition<'a> {
        if self.remote_absent {
            Precondition::MustNotExist
        } else {
            Precondition::from_etag(etag)
        }
    }
}

/// 本機檔案相對上次同步記錄是否已變更（只讀本機）。
///
/// 供 [`evaluate`] 與「評估後、下載前」的重新核對共用：下載計畫是在 `HEAD`
/// 評估時定的，而評估到暫停生效之間使用者仍可能寫入本機檔案；下載前以同一個
/// 判準重驗，就能避免用雲端覆寫剛保存的修改。
pub(crate) fn local_changed(local_path: &Path, record: &FileRecord) -> AppResult<bool> {
    Ok(local_differs(read_local(local_path)?.as_deref(), record))
}

/// 由已讀出的本機內容判斷是否變更（純函式）。
fn local_differs(local: Option<&[u8]>, record: &FileRecord) -> bool {
    match (local, &record.hash) {
        (Some(bytes), Some(hash)) => fingerprint(bytes) != hash.as_str(),
        // 記錄沒有指紋（從未同步）卻有本機檔：視為已變更。
        (Some(_), None) => true,
        // 本機沒有這個檔案：不算變更（有沒有東西可同步由 [`evaluate`] 決定）。
        (None, _) => false,
    }
}

/// 遠端相對上次同步記錄是否已變更（純函式）。
///
/// 遠端檔案**不存在**也算「已變更」：上次同步之後它從雲端消失了（或在新的
/// 伺服器上根本沒有），這是與上次不同的狀態。若把它當成「沒動」，本機未變更
/// 時整個同步就成了 [`Plan::Noop`]——「立即同步」回報成功卻什麼都沒上傳，
/// 使用者換伺服器或雲端檔案被刪除後都會踩到。
fn remote_differs(remote: &RemoteMeta, record: &FileRecord) -> bool {
    match (remote_version(remote), &record.version) {
        (Some(current), Some(previous)) => current != previous.as_str(),
        // 遠端有內容而上次沒有版本記錄（首次同步）：有檔案即有變更。
        (Some(_), None) => remote.exists,
        // 上次有版本、現在遠端不存在：被刪除。
        (None, Some(_)) => !remote.exists,
        // 兩邊都沒有版本可比（伺服器不回 `ETag`／`Last-Modified`）：無法偵測。
        (None, None) => false,
    }
}

/// 遠端不存在時的計畫：本機有檔且任一邊有變更，就由本機補上。
///
/// 遠端已經沒有內容可下載，而這兩個檔案（憑證／任務）都是本機一定會保留的
/// 資料——上傳只是把雲端的鏡像補回來，不會覆蓋雲端的任何內容，因此不需要
/// 使用者介入（`remote_absent` 會讓前置條件成為「必須不存在」，其他裝置同時
/// 補上時仍會以衝突收場）。兩邊都沒有變更（本機沒有檔、或無法得知遠端版本）
/// 時維持 [`Plan::Noop`]。
fn plan_absent_remote(local_present: bool, changed: bool) -> Plan {
    if local_present && changed {
        Plan::Push
    } else {
        Plan::Noop
    }
}

/// 評估單一檔案的同步計畫（只讀不寫）。
pub(crate) fn evaluate(
    webdav: &WebDav,
    file: SyncFile,
    local_path: &Path,
    record: &FileRecord,
) -> AppResult<Evaluation> {
    let local = read_local(local_path)?;
    let local_changed = local_differs(local.as_deref(), record);
    let remote = webdav.head(file.remote_name())?;
    let remote_changed = remote_differs(&remote, record);
    let plan = if remote.exists {
        plan(local_changed, remote_changed)
    } else {
        plan_absent_remote(local.is_some(), local_changed || remote_changed)
    };
    Ok(Evaluation {
        plan,
        remote_absent: !remote.exists,
    })
}

/// 下載遠端檔案覆蓋本機，回傳新的同步記錄；遠端缺此檔時回 `None`。
pub(crate) fn pull(
    webdav: &WebDav,
    file: SyncFile,
    local_path: &Path,
) -> AppResult<Option<FileRecord>> {
    let Some((bytes, meta)) = webdav.get(file.remote_name())? else {
        return Ok(None);
    };
    ensure_container(file, &bytes)?;
    io::write_private_atomic(local_path, &bytes)?;
    Ok(Some(FileRecord {
        version: remote_version(&meta).map(str::to_owned),
        etag: meta.if_match.clone(),
        hash: Some(fingerprint(&bytes)),
    }))
}

/// 上傳本機檔案覆蓋遠端，回傳新的同步記錄。
///
/// `precondition` 為上傳的前置條件（見 [`Precondition`]）：帶入 [`Precondition::Match`]
/// 時 PUT 成為條件請求，若遠端在檢查後又被其他裝置改動，伺服器會回 `412`
///（映射為 [`AppError::WebDavConflict`]），呼叫端可據此避免覆蓋而改報衝突；
/// 遠端確認不存在時用 [`Precondition::MustNotExist`]，避免首次建立互撞。
pub(crate) fn push(
    webdav: &WebDav,
    file: SyncFile,
    local_path: &Path,
    precondition: Precondition<'_>,
) -> AppResult<FileRecord> {
    let Some(bytes) = read_local(local_path)? else {
        return Err(AppError::webdav(format!(
            "本机没有 {} 可供上传",
            file.remote_name()
        )));
    };
    let name = file.remote_name();
    let meta = webdav.put(name, &bytes, precondition)?;
    // 伺服器未在 PUT 回應提供版本時，補做一次 HEAD 取得權威版本。
    let meta = if remote_version(&meta).is_none() {
        webdav.head(name)?
    } else {
        meta
    };
    Ok(FileRecord {
        version: remote_version(&meta).map(str::to_owned),
        etag: meta.if_match.clone(),
        hash: Some(fingerprint(&bytes)),
    })
}

/// 導入：下載全部遠端檔案覆寫本機，回傳各檔案的同步記錄。
///
/// 用於登入畫面的「從堅果雲導入」——此時尚無加密口令，無法讀寫 `sync.vault`，
/// 因此只回傳記錄，待解鎖後再存回。遠端缺少個別檔案時略過；一個都沒有時回錯。
///
/// **先全部下載並驗證、再一起落盤**：任一檔案下載失敗或不是本程式的容器時，
/// 本機檔案完全不動。若邊下載邊寫入，中途失敗會留下「一半新一半舊」的本機檔，
/// 而導入又不儲存記錄，之後的同步會以錯誤的基準比對。
pub(crate) fn import(
    webdav: &WebDav,
    targets: &[(SyncFile, PathBuf)],
) -> AppResult<Vec<(SyncFile, FileRecord)>> {
    let mut fetched: Vec<(SyncFile, &PathBuf, Vec<u8>, RemoteMeta)> = Vec::new();
    for (file, path) in targets {
        let name = file.remote_name();
        let Some((bytes, meta)) = webdav.get(name)? else {
            continue;
        };
        ensure_container(*file, &bytes)?;
        fetched.push((*file, path, bytes, meta));
    }
    if fetched.is_empty() {
        return Err(AppError::webdav("云端没有可导入的同步数据"));
    }
    // 全部驗證通過後才落盤。
    let mut records = Vec::new();
    for (file, path, bytes, meta) in &fetched {
        io::write_private_atomic(path, bytes)?;
        records.push((
            *file,
            FileRecord {
                version: remote_version(meta).map(str::to_owned),
                etag: meta.if_match.clone(),
                hash: Some(fingerprint(bytes)),
            },
        ));
    }
    Ok(records)
}

/// 下載內容必須是結構完整的加密信封，才允許覆寫本機檔案。
///
/// 遠端檔名固定，取回的內容卻未必是本程式的容器（指到錯的目錄、雲端上的
/// 同名檔案、供應商回的錯誤頁）。直接覆寫會讓本機仍可用的憑證或任務檔消失，
/// 因此寧可整筆失敗也不寫入。這裡只驗結構——AAD 與標籤的認證要等口令到齊。
fn ensure_container(file: SyncFile, bytes: &[u8]) -> AppResult<()> {
    if envelope::is_envelope(bytes) {
        return Ok(());
    }
    Err(AppError::webdav(format!(
        "云端文件 {} 不是本程序加密的容器，已保留本机文件（请确认服务器地址与账户指向正确的同步目录）",
        file.remote_name()
    )))
}

/// 讀取本機檔案；不存在時回 `None`。
fn read_local(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match io::read_private(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(AppError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// 取出遠端版本識別：優先用 `ETag`，否則退回 `Last-Modified`。
fn remote_version(meta: &RemoteMeta) -> Option<&str> {
    meta.etag.as_deref().or(meta.last_modified.as_deref())
}

#[cfg(test)]
#[path = "tests/engine_test.rs"]
mod engine_test;
