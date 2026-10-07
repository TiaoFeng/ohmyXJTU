//! 站點客戶端：考勤系統與思源學堂。

pub mod attendance;
pub mod lms;

use serde::Deserialize as _;
use serde::de::DeserializeOwned;

use crate::error::{AppError, AppResult};
use crate::http::HttpResponse;

/// 確認登入收尾取得的是有效回應（非維護頁或登入頁）。
///
/// 站點的登入收尾（例如思源學堂的 `/user/index`）即使遇到 5xx 維護頁或被導回
/// 登入頁，仍會回傳可讀的 HTML。不檢查就會把站點標記為已登入：之後的查詢
/// 全部以「待核实」收場，使用者卻只看到「登入成功」。
pub fn ensure_authenticated(response: &HttpResponse) -> AppResult<()> {
    if response.status >= 500 {
        return Err(AppError::Http {
            status: response.status,
        });
    }
    if crate::session::site::is_auth_failure(response) {
        return Err(AppError::SessionExpired);
    }
    response.error_for_status()
}

/// 解開 `{code, message, data}` 外殼，並把 `data` 解析為指定型別。
///
/// 外殼解碼規則（嚴格度）集中於 [`crate::json::split_envelope`]；缺 `code`
/// 或型別不符一律視為協定格式錯誤，不可用假業務碼回報。
pub fn unwrap_envelope<T: DeserializeOwned>(
    response: &HttpResponse,
    context: &str,
) -> AppResult<T> {
    let data = crate::json::split_envelope(response, context)?;
    deserialize_value(data, context)
}

/// 解析純 JSON 回應（無外殼）。
pub fn parse_json<T: DeserializeOwned>(response: &HttpResponse, context: &str) -> AppResult<T> {
    response.error_for_status()?;
    let value: serde_json::Value = response.json()?;
    deserialize_value(value, context)
}

/// 將 JSON 值解析為指定型別。
///
/// 錯誤訊息只描述失敗類別（不含原始欄位值），避免回應內容出現在介面提示中。
pub fn deserialize_value<T: DeserializeOwned>(
    value: serde_json::Value,
    context: &str,
) -> AppResult<T> {
    serde_json::from_value(value).map_err(|err| {
        AppError::protocol(format!(
            "{context} 响应格式不符（{}）",
            crate::error::describe_json_failure(err.classify())
        ))
    })
}

/// JSON 值的型別名稱（不含內容）。
fn value_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "空值",
        serde_json::Value::Bool(_) => "布尔值",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::Object(_) => "对象",
    }
}

/// 逐項解析列表，回傳（成功項目, 被跳過的項目數）。
///
/// 伺服器對列表項的欄位並不總是完整（例如缺少活動 ID），
/// 個別項目解析失敗時跳過該項，而不是讓整份列表查詢失敗。
pub fn parse_lenient<T: DeserializeOwned>(
    items: serde_json::Value,
    context: &str,
) -> AppResult<(Vec<T>, usize)> {
    let serde_json::Value::Array(items) = items else {
        return Err(AppError::protocol(format!("{context} 响应不是列表")));
    };

    let mut parsed = Vec::with_capacity(items.len());
    let mut skipped = 0_usize;
    for item in items {
        match serde_json::from_value::<T>(item) {
            Ok(value) => parsed.push(value),
            Err(_) => skipped += 1,
        }
    }
    Ok((parsed, skipped))
}

/// 接受字串或數字的欄位（伺服器對識別碼與節次的型別並不統一）。
pub(crate) fn string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "期望字符串或数字，实际为{}",
            value_type_name(&other)
        ))),
    }
}

/// 接受數字或可解析為數字的字串。
pub(crate) fn u32_or_string<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match &value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| serde::de::Error::custom("数值超出范围")),
        serde_json::Value::String(text) => text
            .trim()
            .parse::<u32>()
            .map_err(|_| serde::de::Error::custom("无法解析为数字")),
        other => Err(serde::de::Error::custom(format!(
            "期望数字或字符串，实际为{}",
            value_type_name(other)
        ))),
    }
}

/// 缺欄位或型別不符時回傳 `0` 的數字欄位（**不報錯**）。
///
/// 用於目前不參與語意判斷的數值欄位（例如課程考勤記錄的 `courseWeek`）：這些
/// 欄位讀不出來時只損失欄位本身，不該讓整筆記錄被 [`parse_lenient`] 丟棄。
pub(crate) fn u32_lenient<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(u32_or_string(deserializer).unwrap_or(0))
}

/// 日期欄位：接受 `YYYY-MM-DD` 與 `YYYY/MM/DD`，一律正規化為 `YYYY-MM-DD`。
///
/// 考勤比對以「日期字串全等」為鍵，因此分隔符一變（`2026/10/07`）就會全數失配，
/// 每一堂已過的課都變成「待核实」——錯得無聲且全面。這裡在解析時就正規化，
/// 呼叫端不必各自處理格式差異。
///
/// 無法解讀時回報錯誤（訊息不含欄位值），該筆記錄由 [`parse_lenient`] 跳過並計數；
/// 留著一個永遠配不上的日期只會讓使用者看到一堆沒有原因的「待核实」。
pub(crate) fn date_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::String(raw)) = value else {
        return Err(serde::de::Error::custom("期望 YYYY-MM-DD 格式的日期"));
    };
    normalize_date(&raw).ok_or_else(|| serde::de::Error::custom("日期格式无法识别"))
}

/// 把日期字串解析為日期（正規化分隔符並驗證形狀）。
///
/// 接受 `-` 與 `/` 分隔、允許前後空白；**形狀必須是 4-2-2 位數字**——chrono 的
/// `%Y` 只要求「一位以上數字」，`09/01/26` 會被讀成公元 9 年，變成一筆看似有效
/// 卻荒謬的記錄（比讀不出來更糟：它會參與比對、也可能讓學期起點跑到兩千年前）。
pub(crate) fn parse_date_lenient(raw: &str) -> Option<chrono::NaiveDate> {
    let normalized = raw.trim().replace('/', "-");
    let mut parts = normalized.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let shaped = [year, month, day];
    if shaped[0].len() != 4 || shaped[1].len() != 2 || shaped[2].len() != 2 {
        return None;
    }
    if !shaped
        .iter()
        .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d").ok()
}

/// 把日期字串正規化為 `YYYY-MM-DD`。
fn normalize_date(raw: &str) -> Option<String> {
    parse_date_lenient(raw).map(|date| date.format("%Y-%m-%d").to_string())
}

/// 缺欄位時回傳 `None`，型別不符時才報錯的可選字串。
pub(crate) fn optional_string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(serde_json::Value::Number(number)) => Ok(Some(number.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "期望字符串或数字，实际为{}",
            value_type_name(&other)
        ))),
    }
}

/// 缺欄位或型別不符時回傳 `None` 的字串或數字欄位（**不報錯**）。
///
/// 與 [`optional_string_or_number`] 的差別是型別異常一律視為缺漏，而不是讓整筆
/// 記錄失敗。用於不參與語意判斷的識別碼欄位（例如提交記錄的 `id`）：讀不出來時
/// 只損失該欄位，不該讓整筆記錄被 [`parse_lenient`] 丟棄——提交記錄被丟棄會讓
/// 「已完成」被誤判成「未提交／逾期」（參考實作對 `id` 也採寬容讀取）。
pub(crate) fn optional_string_or_number_lenient<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(serde_json::Value::Number(number)) => Ok(Some(number.to_string())),
        _ => Ok(None),
    }
}

/// 缺欄位或型別不符時回傳 `None` 的可選字串。
///
/// 只接受字串：非字串（數字、物件、陣列…）一律視為「沒有這段內容」。用於純展示
/// 用的文字欄位（例如活動說明）：這些欄位型別異常時只應損失該段說明，不應讓整份
/// 回應解析失敗——詳情解析失敗會使該課程的作業全部退回「待核实」。
pub(crate) fn optional_string_lenient<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        _ => Ok(None),
    }
}

/// 缺欄位或型別不符時回傳空字串的字串欄位（**不報錯**）。
///
/// 接受字串與數字（與 [`string_or_number`] 同一組型別，識別碼偶爾以數字回傳），
/// 其餘型別一律視為空字串。用於「整項資料不該因為這一個欄位而消失」的欄位
/// （例如活動的類型 `type`）：型別異常時只損失該欄位的內容，項目本身仍保留，
/// 由呼叫端以預設值呈現。相對地 `optional_string_lenient` 用於「沒有內容」等同
/// 缺漏的欄位（例如活動說明）。
pub(crate) fn string_lenient<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) => text,
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::new(),
    })
}

/// 缺欄位或型別不符時回傳 `None` 的非負整數（接受數字與可解析的數字字串）。
///
/// 用於純展示用的數量欄位（例如附件大小）：型別異常時只損失這個數字，不讓整份
/// 回應解析失敗。
pub(crate) fn optional_u64_lenient<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let number = match value {
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    Ok(number)
}

/// 寬容布林：接受布林、`0`/`1` 與其字串形式（`true`／`false`／`yes`／`y`／
/// `no`／`n`）；缺欄位或型別不符時回 `None`。
///
/// 伺服器對布林欄位的型別並不統一（考勤流水與思源學堂都出現過字串形式），
/// 型別異常不該讓整頁解析失敗；參考實作以同一套寬容規則讀取（`KqHttp.bool`）。
pub(crate) fn lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::Bool(flag) => Some(flag),
        serde_json::Value::Number(number) => number.as_i64().map(|number| number != 0),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => Some(true),
            "false" | "0" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }))
}

/// 寬容旗標：與 [`lenient_bool`] 同一套規則，但缺少或無法解讀時視為 `false`。
///
/// 用於「預設為否」的旗標欄位（例如考勤流水的 `effective`）：伺服器沒給或給了
/// 看不懂的值時，保守地不宣稱該筆記錄有效。
pub(crate) fn lenient_flag<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_bool(deserializer)?.unwrap_or(false))
}

/// 缺欄位、不是陣列或個別項目解析失敗時都不報錯的列表欄位。
///
/// 逐項解析並跳過失敗的項目（與 [`parse_lenient`] 同精神）：附件清單異常不該讓
/// 整份活動詳情失敗——詳情失敗會使該課程的作業全部退回「待核实」。
pub(crate) fn lenient_array<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Array(items)) = value else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect())
}

#[cfg(test)]
#[path = "tests/mod_test.rs"]
mod mod_test;
