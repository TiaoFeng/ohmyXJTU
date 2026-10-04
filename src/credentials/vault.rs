//! 加密憑證保險庫。
//!
//! 檔案格式與金鑰派生見 [`crate::credentials::envelope`]；本模組只負責憑證的
//! 序列化、檔案路徑語意與使用者可見的錯誤訊息：
//!
//! - AAD 前綴為 `ohmyXJTU-vault`（與任務檔的 `ohmyXJTU-tasks` 區隔，兩者
//!   無法互相冒充）。
//! - 解密驗證失敗時只會回報 [`AppError::WrongPassphrase`]——口令錯誤與檔案
//!   竄改在 AEAD 下無法區分，也不需要區分。
//! - 檔案結構損毀時錯誤訊息附上路徑與刪除重建的指引（不含檔案內容）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::credentials::envelope;
use crate::error::{AppError, AppResult};
use crate::io;

/// 憑證檔的 AAD 前綴（完整 AAD 由前綴與檔案格式版本組成）。
const AAD_PREFIX: &str = "ohmyXJTU-vault";

/// 帳號憑證。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// 學號、手機號或信箱。
    pub username: String,
    /// 統一認證密碼。
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    /// 只輸出遮罩：任何 `{:?}` 都不會洩漏帳號或密碼。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Credentials {
    /// 建立憑證。
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    /// 使用者名稱與密碼是否皆非空。
    pub fn is_complete(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.is_empty()
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
    }
}

/// 憑證保險庫。
#[derive(Debug, Clone)]
pub struct Vault {
    path: PathBuf,
}

impl Vault {
    /// 使用標準資料目錄下的憑證檔。
    pub fn at_default_path() -> AppResult<Self> {
        Ok(Self {
            path: io::vault_path()?,
        })
    }

    /// 使用指定路徑（便於測試與未來的多設定檔支援）。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 憑證檔路徑。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 憑證檔是否已存在。
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// 以口令加密並原子寫入憑證。
    pub fn store(&self, passphrase: &str, credentials: &Credentials) -> AppResult<()> {
        let bytes = encrypt(passphrase, credentials)?;
        io::write_private_atomic(&self.path, &bytes)
    }

    /// 以口令解密讀取憑證。
    ///
    /// 檔案結構損毀時任何口令都無法解鎖；錯誤訊息附上檔案路徑與刪除
    /// 重建的指引（不含檔案內容），讓使用者有恢復途徑。
    pub fn load(&self, passphrase: &str) -> AppResult<Credentials> {
        let bytes = io::read_private(&self.path)?;
        decrypt(passphrase, &bytes).map_err(|err| self.describe_file_error(err))
    }

    /// 為結構損毀錯誤補上路徑與重建指引；其餘錯誤（如口令錯誤）原樣回傳。
    fn describe_file_error(&self, err: AppError) -> AppError {
        match err {
            AppError::VaultFile(detail) => AppError::VaultFile(format!(
                "{detail}；可删除 {} 后重新设置凭证",
                self.path.display()
            )),
            other => other,
        }
    }

    /// 更換口令：先以舊口令解密（同時驗證舊口令），再以新口令重新加密寫入。
    pub fn change_passphrase(&self, old: &str, new: &str) -> AppResult<Credentials> {
        let credentials = self.load(old)?;
        self.store(new, &credentials)?;
        Ok(credentials)
    }
}

fn encrypt(passphrase: &str, credentials: &Credentials) -> AppResult<Vec<u8>> {
    let plaintext = Zeroizing::new(
        serde_json::to_vec(credentials)
            .map_err(|err| AppError::Crypto(format!("凭证序列化失败：{err}")))?,
    );
    let (bytes, _sealed) = envelope::seal(passphrase, AAD_PREFIX, &plaintext)?;
    Ok(bytes)
}

fn decrypt(passphrase: &str, bytes: &[u8]) -> AppResult<Credentials> {
    let (plaintext, _sealed) = envelope::unseal(passphrase, AAD_PREFIX, bytes)?;
    // 錯誤訊息只描述類別：`serde_json` 的型別錯誤會引用出問題的欄位值。
    serde_json::from_slice(&plaintext).map_err(|err| {
        AppError::VaultFile(format!(
            "凭证内容无法解析（{}）",
            crate::error::describe_json_failure(err.classify())
        ))
    })
}

#[cfg(test)]
#[path = "tests/vault_test.rs"]
mod vault_test;
