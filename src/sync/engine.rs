//! 同步引擎：三方比對與上傳／下載。
//!
//! 只做檔案層面的操作，不碰介面或通道；由工作者（見 `crate::task::worker`）呼叫。
//! 「三方比對」指的是本機現況、遠端現況與上次同步的記錄：據此判斷只有一邊變更
//!（安全地單向上傳或下載），或兩邊都變更（衝突，不自動覆蓋）。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::io;
use crate::sync::config::{FileRecord, SyncFile};
use crate::sync::webdav::{RemoteMeta, WebDav};

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

/// 評估單一檔案的同步計畫（只讀不寫）。
pub(crate) fn evaluate(
    webdav: &WebDav,
    file: SyncFile,
    local_path: &Path,
    record: &FileRecord,
) -> AppResult<Plan> {
    let local_changed = match (read_local(local_path)?, &record.hash) {
        (Some(bytes), Some(hash)) => &fingerprint(&bytes) != hash,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let remote = webdav.head(file.remote_name())?;
    let remote_changed = match (remote_version(&remote), &record.version) {
        (Some(current), Some(previous)) => current != previous.as_str(),
        (Some(_), None) => remote.exists,
        (None, _) => false,
    };
    Ok(plan(local_changed, remote_changed))
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
    io::write_private_atomic(local_path, &bytes)?;
    Ok(Some(FileRecord {
        version: remote_version(&meta).map(str::to_owned),
        etag: meta.if_match.clone(),
        hash: Some(fingerprint(&bytes)),
    }))
}

/// 上傳本機檔案覆蓋遠端，回傳新的同步記錄。
///
/// `if_match` 為上次同步記錄的遠端 `ETag`：帶入後 PUT 成為條件請求，若遠端在
/// 檢查後又被其他裝置改動，伺服器會回 `412`（映射為 [`AppError::WebDavConflict`]），
/// 呼叫端可據此避免覆蓋而改報衝突。遠端尚無檔案（首次上傳）或無 ETag 時帶 `None`。
pub(crate) fn push(
    webdav: &WebDav,
    file: SyncFile,
    local_path: &Path,
    if_match: Option<&str>,
) -> AppResult<FileRecord> {
    let Some(bytes) = read_local(local_path)? else {
        return Err(AppError::webdav(format!(
            "本机没有 {} 可供上传",
            file.remote_name()
        )));
    };
    let name = file.remote_name();
    let meta = webdav.put(name, &bytes, if_match)?;
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
pub(crate) fn import(
    webdav: &WebDav,
    targets: &[(SyncFile, PathBuf)],
) -> AppResult<Vec<(SyncFile, FileRecord)>> {
    let mut records = Vec::new();
    for (file, path) in targets {
        let name = file.remote_name();
        let Some((bytes, meta)) = webdav.get(name)? else {
            continue;
        };
        io::write_private_atomic(path, &bytes)?;
        records.push((
            *file,
            FileRecord {
                version: remote_version(&meta).map(str::to_owned),
                etag: meta.if_match.clone(),
                hash: Some(fingerprint(&bytes)),
            },
        ));
    }
    if records.is_empty() {
        return Err(AppError::webdav("云端没有可导入的同步数据"));
    }
    Ok(records)
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
