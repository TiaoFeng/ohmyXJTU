//! 加密憑證保險庫。
//!
//! 檔案格式（皆為小端序）：
//!
//! | 欄位 | 長度 | 說明 |
//! | --- | --- | --- |
//! | `magic` | 8 | `OMXJTU1\n` |
//! | `version` | 1 | 格式版本，目前為 `1` |
//! | `m_cost` | 4 | Argon2id 記憶體成本（KiB） |
//! | `t_cost` | 4 | Argon2id 迭代次數 |
//! | `p_cost` | 4 | Argon2id 平行度 |
//! | `salt` | 16 | 隨機鹽值 |
//! | `nonce` | 12 | AEAD nonce |
//! | `ciphertext` | 其餘 | ChaCha20-Poly1305 密文（含 16 位元組 tag） |
//!
//! 金鑰由 Argon2id 自使用者口令與鹽值派生；AAD 綁定格式版本，
//! 因此解密驗證失敗時只會回報 [`AppError::WrongPassphrase`]——
//! 口令錯誤與檔案竄改在 AEAD 下無法區分，也不需要區分。

use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{AppError, AppResult};
use crate::io;
use crate::random;

const MAGIC: &[u8; 8] = b"OMXJTU1\n";
const VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;

/// Argon2id 記憶體成本（KiB）：64 MiB。
const M_COST: u32 = 64 * 1024;
/// Argon2id 迭代次數。
const T_COST: u32 = 3;
/// Argon2id 平行度。
const P_COST: u32 = 1;

/// 讀取憑證檔時允許的最大 KDF 參數，避免惡意檔案導致資源耗盡。
const MAX_M_COST: u32 = 1024 * 1024;
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 8;

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

#[derive(Debug, Clone, Copy)]
struct KdfParams {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

const DEFAULT_KDF: KdfParams = KdfParams {
    m_cost: M_COST,
    t_cost: T_COST,
    p_cost: P_COST,
};

/// AEAD 的附加驗證資料：由檔案格式版本派生，因此版本欄位受完整性保護。
///
/// 升版時 AAD 會隨 [`VERSION`] 自動改變，不會與版本常數脫節；現行 v1 的
/// 輸出與舊常數 `b"ohmyXJTU-vault-v1"` 位元組相同，舊保險庫仍可解密。
fn aad(version: u8) -> Vec<u8> {
    format!("ohmyXJTU-vault-v{version}").into_bytes()
}

fn encrypt(passphrase: &str, credentials: &Credentials) -> AppResult<Vec<u8>> {
    let plaintext = Zeroizing::new(
        serde_json::to_vec(credentials)
            .map_err(|err| AppError::Crypto(format!("凭证序列化失败：{err}")))?,
    );

    let mut salt = [0_u8; SALT_LEN];
    let mut nonce = [0_u8; NONCE_LEN];
    random::fill(&mut salt)?;
    random::fill(&mut nonce)?;

    let key = derive_key(passphrase, DEFAULT_KDF, &salt)?;
    let cipher = ChaCha20Poly1305::new((&*key).into());
    let aad = aad(VERSION);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| AppError::Crypto("凭证加密失败".to_owned()))?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&DEFAULT_KDF.m_cost.to_le_bytes());
    out.extend_from_slice(&DEFAULT_KDF.t_cost.to_le_bytes());
    out.extend_from_slice(&DEFAULT_KDF.p_cost.to_le_bytes());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(passphrase: &str, bytes: &[u8]) -> AppResult<Credentials> {
    let (version, params, salt, nonce, ciphertext) = parse(bytes)?;
    let key = derive_key(passphrase, params, &salt)?;
    let cipher = ChaCha20Poly1305::new((&*key).into());
    let aad = aad(version);
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| AppError::WrongPassphrase)?;
    let plaintext = Zeroizing::new(plaintext);
    // 錯誤訊息只描述類別：`serde_json` 的型別錯誤會引用出問題的欄位值。
    serde_json::from_slice(&plaintext).map_err(|err| {
        AppError::VaultFile(format!(
            "凭证内容无法解析（{}）",
            crate::error::describe_json_failure(err.classify())
        ))
    })
}

type ParsedVault<'a> = (u8, KdfParams, [u8; SALT_LEN], [u8; NONCE_LEN], &'a [u8]);

fn parse(bytes: &[u8]) -> AppResult<ParsedVault<'_>> {
    if bytes.len() < HEADER_LEN + TAG_LEN {
        return Err(AppError::VaultFile("凭证文件长度不足".to_owned()));
    }
    let magic = bytes
        .get(..MAGIC.len())
        .ok_or_else(|| AppError::VaultFile("凭证文件标识不完整".to_owned()))?;
    if magic != MAGIC {
        return Err(AppError::VaultFile("凭证文件标识不匹配".to_owned()));
    }

    let mut offset = MAGIC.len();
    let version = bytes[offset];
    offset += 1;
    if version != VERSION {
        return Err(AppError::VaultFile(format!(
            "不支持的凭证文件版本：{version}"
        )));
    }

    let params = KdfParams {
        m_cost: read_u32(bytes, &mut offset)?,
        t_cost: read_u32(bytes, &mut offset)?,
        p_cost: read_u32(bytes, &mut offset)?,
    };
    if params.m_cost > MAX_M_COST || params.t_cost > MAX_T_COST || params.p_cost > MAX_P_COST {
        return Err(AppError::VaultFile("凭证文件的派生参数超出限制".to_owned()));
    }

    let salt = read_array::<SALT_LEN>(bytes, &mut offset)?;
    let nonce = read_array::<NONCE_LEN>(bytes, &mut offset)?;
    let ciphertext = &bytes[offset..];
    Ok((version, params, salt, nonce, ciphertext))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> AppResult<u32> {
    Ok(u32::from_le_bytes(read_array::<4>(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: &mut usize) -> AppResult<[u8; N]> {
    let end = *offset + N;
    let slice = bytes
        .get(*offset..end)
        .ok_or_else(|| AppError::VaultFile("凭证文件头不完整".to_owned()))?;
    *offset = end;
    slice
        .try_into()
        .map_err(|_| AppError::VaultFile("凭证文件头不完整".to_owned()))
}

fn derive_key(
    passphrase: &str,
    params: KdfParams,
    salt: &[u8],
) -> AppResult<Zeroizing<[u8; KEY_LEN]>> {
    let params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(KEY_LEN))
        .map_err(|err| AppError::Crypto(format!("口令派生参数无效：{err}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|err| AppError::Crypto(format!("口令派生失败：{err}")))?;
    Ok(key)
}

#[cfg(test)]
#[path = "tests/vault_test.rs"]
mod vault_test;
