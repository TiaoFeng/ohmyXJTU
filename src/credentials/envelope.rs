//! 加密信封：口令派生金鑰與 AEAD 的檔案原語。
//!
//! 憑證保險庫與任務檔共用同一套格式與 KDF，只有 AAD 前綴不同——兩者是不同的
//! 檔案種類，把其中之一改名成另一種也無法解開（AAD 不符即認證失敗）。
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
//! 金鑰由 Argon2id 自使用者口令與鹽值派生；AAD 由呼叫端提供的前綴與檔案
//! 格式版本組成（`{prefix}-v{version}`），因此解密驗證失敗時呼叫端只需要
//! 回報「口令錯誤或檔案已損壞」——兩者在 AEAD 下無法區分，也不需要區分。
//!
//! [`Sealed`] 保留解開信封時派生的金鑰，讓同一會話中的後續寫入不必重新
//! 派生（Argon2id 刻意昂貴）；[`reseal`] 會**沿用同一鹽值與參數**，否則下
//! 次以口令解鎖時派生不出同一把金鑰。金鑰離開作用域即零化。

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::random;

/// 信封格式標識。
pub(crate) const MAGIC: &[u8; 8] = b"OMXJTU1\n";
/// 信封格式版本。
pub(crate) const VERSION: u8 = 1;
/// 鹽值長度。
pub(crate) const SALT_LEN: usize = 16;
/// AEAD nonce 長度。
pub(crate) const NONCE_LEN: usize = 12;
/// 派生金鑰長度。
const KEY_LEN: usize = 32;
/// AEAD tag 長度。
const TAG_LEN: usize = 16;
/// 檔頭長度（不含密文）。
pub(crate) const HEADER_LEN: usize = MAGIC.len() + 1 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;

/// Argon2id 記憶體成本（KiB）：64 MiB。
const M_COST: u32 = 64 * 1024;
/// Argon2id 迭代次數。
const T_COST: u32 = 3;
/// Argon2id 平行度。
const P_COST: u32 = 1;

/// 讀取檔案時允許的最大 KDF 參數，避免惡意檔案導致資源耗盡。
const MAX_M_COST: u32 = 1024 * 1024;
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 8;

/// 口令派生參數。
#[derive(Debug, Clone, Copy)]
pub(crate) struct KdfParams {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

/// 新檔案使用的預設派生參數。
pub(crate) const DEFAULT_KDF: KdfParams = KdfParams {
    m_cost: M_COST,
    t_cost: T_COST,
    p_cost: P_COST,
};

/// 已解開的信封：保留派生金鑰與其派生參數，供同一會話的後續寫入重用。
pub(crate) struct Sealed {
    params: KdfParams,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for Sealed {
    /// 只輸出遮罩：任何 `{:?}` 都不會洩漏派生金鑰。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sealed")
            .field("key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// AEAD 的附加驗證資料：由檔案種類前綴與格式版本派生。
///
/// 升版時 AAD 會隨版本自動改變，不會與版本常數脫節。
pub(crate) fn aad(prefix: &str, version: u8) -> Vec<u8> {
    format!("{prefix}-v{version}").into_bytes()
}

/// 以口令派生新金鑰、加密內容並組出完整檔案位元組。
pub(crate) fn seal(
    passphrase: &str,
    prefix: &str,
    plaintext: &[u8],
) -> AppResult<(Vec<u8>, Sealed)> {
    let mut salt = [0_u8; SALT_LEN];
    let mut nonce = [0_u8; NONCE_LEN];
    random::fill(&mut salt)?;
    random::fill(&mut nonce)?;

    let key = derive_key(passphrase, DEFAULT_KDF, &salt)?;
    let ciphertext = encrypt_with(&key, prefix, VERSION, &nonce, plaintext)?;
    let bytes = build_file(DEFAULT_KDF, &salt, &nonce, &ciphertext);
    Ok((
        bytes,
        Sealed {
            params: DEFAULT_KDF,
            salt,
            key,
        },
    ))
}

/// 使用既有的信封重新加密（沿用同一鹽值與派生參數，只換 nonce）。
///
/// 不得重新產生鹽值：鹽值一旦改變，下次以口令解鎖時派生出的金鑰會與
/// [`Self`] 中的金鑰不同，舊內容將無法解開。
pub(crate) fn reseal(sealed: &Sealed, prefix: &str, plaintext: &[u8]) -> AppResult<Vec<u8>> {
    let mut nonce = [0_u8; NONCE_LEN];
    random::fill(&mut nonce)?;
    let ciphertext = encrypt_with(&sealed.key, prefix, VERSION, &nonce, plaintext)?;
    Ok(build_file(sealed.params, &sealed.salt, &nonce, &ciphertext))
}

/// 以口令解開信封，回傳明文與可重用的金鑰控制代碼。
pub(crate) fn unseal(
    passphrase: &str,
    prefix: &str,
    bytes: &[u8],
) -> AppResult<(Zeroizing<Vec<u8>>, Sealed)> {
    let (params, salt, nonce, ciphertext) = parse(bytes)?;
    let key = derive_key(passphrase, params, &salt)?;
    let aad = aad(prefix, VERSION);
    let cipher = ChaCha20Poly1305::new((&*key).into());
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| AppError::WrongPassphrase)?;
    Ok((Zeroizing::new(plaintext), Sealed { params, salt, key }))
}

type ParsedEnvelope<'a> = (KdfParams, [u8; SALT_LEN], [u8; NONCE_LEN], &'a [u8]);

fn encrypt_with(
    key: &[u8; KEY_LEN],
    prefix: &str,
    version: u8,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> AppResult<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let aad = aad(prefix, version);
    cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| AppError::Crypto("加密失败".to_owned()))
}

fn build_file(
    params: KdfParams,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&params.m_cost.to_le_bytes());
    out.extend_from_slice(&params.t_cost.to_le_bytes());
    out.extend_from_slice(&params.p_cost.to_le_bytes());
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);
    out.extend_from_slice(ciphertext);
    out
}

fn parse(bytes: &[u8]) -> AppResult<ParsedEnvelope<'_>> {
    if bytes.len() < HEADER_LEN + TAG_LEN {
        return Err(AppError::VaultFile("加密文件长度不足".to_owned()));
    }
    let magic = bytes
        .get(..MAGIC.len())
        .ok_or_else(|| AppError::VaultFile("加密文件标识不完整".to_owned()))?;
    if magic != MAGIC {
        return Err(AppError::VaultFile("加密文件标识不匹配".to_owned()));
    }

    let mut offset = MAGIC.len();
    let version = bytes[offset];
    offset += 1;
    if version != VERSION {
        return Err(AppError::VaultFile(format!(
            "不支持的加密文件版本：{version}"
        )));
    }

    let params = KdfParams {
        m_cost: read_u32(bytes, &mut offset)?,
        t_cost: read_u32(bytes, &mut offset)?,
        p_cost: read_u32(bytes, &mut offset)?,
    };
    if params.m_cost > MAX_M_COST || params.t_cost > MAX_T_COST || params.p_cost > MAX_P_COST {
        return Err(AppError::VaultFile("加密文件的派生参数超出限制".to_owned()));
    }

    let salt = read_array::<SALT_LEN>(bytes, &mut offset)?;
    let nonce = read_array::<NONCE_LEN>(bytes, &mut offset)?;
    let ciphertext = &bytes[offset..];
    Ok((params, salt, nonce, ciphertext))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> AppResult<u32> {
    Ok(u32::from_le_bytes(read_array::<4>(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: &mut usize) -> AppResult<[u8; N]> {
    let end = *offset + N;
    let slice = bytes
        .get(*offset..end)
        .ok_or_else(|| AppError::VaultFile("加密文件头不完整".to_owned()))?;
    *offset = end;
    slice
        .try_into()
        .map_err(|_| AppError::VaultFile("加密文件头不完整".to_owned()))
}

/// 以 Argon2id 自口令與鹽值派生金鑰。
pub(crate) fn derive_key(
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
#[path = "tests/envelope_test.rs"]
mod envelope_test;
