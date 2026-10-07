//! 思源學堂頁面的 JavaScript 物件解析。
//!
//! 真實頁面（例如 `/user/index`）的 `globalData` 是 JavaScript 物件語法：
//! 鍵名不一定加引號、以 `None` 表示空值、可能出現尾逗號，嚴格 JSON 解析
//! 會失敗。參考實作（`ref/lms/lms.py::_parse_js_object`）以正規式預處理後
//! 交給 `json.loads`；這裡以等價的寬容解析器直接讀取所需欄位（不新增依賴）。
//!
//! 解析失敗一律回傳 `None`，錯誤訊息不含頁面內容。

use serde_json::{Map, Number, Value};

/// 依名稱尋找 `name = …`／`name : …` 之後的值並寬容解析。
///
/// 用於 `var globalData = {…}` 這類指派語句；找不到或解析失敗回 `None`。
pub fn find_named_value(text: &str, name: &str) -> Option<Value> {
    let mut from = 0;
    while let Some(at) = find_ident(text, name, from) {
        let pos = skip_ws(text, at + name.len());
        // `name: …` 或 `name = …`（排除比較運算子 `==`）。
        let separated = text[pos..].starts_with(':')
            || (text[pos..].starts_with('=') && !text[pos..].starts_with("=="));
        if !separated {
            from = at + name.len();
            continue;
        }
        let mut parser = Parser::new(text, pos + 1);
        if let Some(value) = parser.parse_value() {
            return Some(value);
        }
        from = at + name.len();
    }
    None
}

/// 取出 `key: {…}, next_key:` 形式的物件（對齊參考實作的邊界條件）。
///
/// 找不到符合邊界的出現位置時回 `None`：寧可回報解析失敗，也不要命中
/// 頁面中其他位置恰好也叫 `key` 的物件。
pub fn parse_js_object(text: &str, key: &str, next_key: &str) -> Option<Value> {
    let mut from = 0;
    while let Some(at) = find_ident(text, key, from) {
        match parse_object_after_key(text, at, key) {
            Some((value, end)) if member_follows(&text[end..], next_key) => return Some(value),
            Some((_, end)) => from = end,
            None => from = at + key.len(),
        }
    }
    None
}

/// 解析 `key` 之後的物件值（允許鍵名帶引號），回傳值與物件結尾位置。
fn parse_object_after_key(text: &str, at: usize, key: &str) -> Option<(Value, usize)> {
    let mut pos = skip_ws(text, at + key.len());
    // 鍵名自身可能帶引號（`"user": {…}`），略過收尾引號。
    if matches!(text[pos..].chars().next(), Some('"') | Some('\'')) {
        pos = skip_ws(text, pos + 1);
    }
    if !text[pos..].starts_with(':') {
        return None;
    }
    let mut parser = Parser::new(text, pos + 1);
    parser.skip_ws();
    if parser.peek() != Some(b'{') {
        return None;
    }
    let value = parser.parse_object()?;
    Some((value, parser.pos))
}

/// `rest` 是否以 `, next_key:` 開頭（容許空白與引號）。
fn member_follows(rest: &str, next_key: &str) -> bool {
    let pos = skip_ws(rest, 0);
    if !rest[pos..].starts_with(',') {
        return false;
    }
    let mut pos = skip_ws(rest, pos + 1);
    if matches!(rest[pos..].chars().next(), Some('"') | Some('\'')) {
        // 鍵名可能帶引號（`, "dept":`）。
        pos += 1;
    }
    let Some(at) = find_ident(rest, next_key, pos) else {
        return false;
    };
    if at != pos {
        return false;
    }
    let mut pos = skip_ws(rest, at + next_key.len());
    if matches!(rest[pos..].chars().next(), Some('"') | Some('\'')) {
        pos = skip_ws(rest, pos + 1);
    }
    rest[pos..].starts_with(':')
}

/// 尋找獨立出現的識別字（前後皆非識別字字元），避免命中 `userNo` 等名稱。
fn find_ident(text: &str, name: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    for (offset, _) in text[from..].match_indices(name) {
        let at = from + offset;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(at);
        }
    }
    None
}

/// 識別字字元（ASCII 字母、數字、`_`、`$`）。
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

/// 略過空白（含 `//` 與 `/* */` 註解），回傳新的位元組索引。
fn skip_ws(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut pos = from;
    while pos < bytes.len() {
        match bytes[pos] {
            b' ' | b'\t' | b'\r' | b'\n' => pos += 1,
            b'/' if bytes.get(pos + 1) == Some(&b'/') => {
                pos += 2;
                while pos < bytes.len() && bytes[pos] != b'\n' {
                    pos += 1;
                }
            }
            b'/' if bytes.get(pos + 1) == Some(&b'*') => {
                pos += 2;
                while pos + 1 < bytes.len() && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                    pos += 1;
                }
                pos = (pos + 2).min(bytes.len());
            }
            _ => break,
        }
    }
    pos
}

/// 極簡 JavaScript 值解析器。
///
/// 支援物件、陣列、單／雙引號字串（含跳脫序列）、數字，以及
/// `true`/`false`/`null`/`None`/`undefined`；無法辨識的內容回 `None`。
struct Parser<'a> {
    text: &'a str,
    pos: usize,
    depth: usize,
}

/// 巢狀結構的最大深度。
///
/// 解析在背景工作執行緒上執行（預設堆疊 2 MiB）：不限制深度的話，異常或惡意
/// 的頁面只要塞入上萬層巢狀容器就會堆疊溢位——那是直接 abort，panic hook 不會
/// 執行（終端可能留在 raw mode）。超限一律回 `None`（呼叫端視為解析失敗，
/// 走既有的「待核实」語意），不嘗試救援異常輸入。
const MAX_DEPTH: usize = 64;

/// 孤立代理（Unicode 代理對缺一半）的替代字元。
///
/// 見 [`Parser::parse_unicode_escape`]：這種字元無法還原成任何合法字元，
/// 但也不能因此讓整段解析失敗。
const REPLACEMENT: char = '\u{fffd}';

impl<'a> Parser<'a> {
    /// 建立解析器。
    fn new(text: &'a str, pos: usize) -> Self {
        Self {
            text,
            pos,
            depth: 0,
        }
    }

    /// 目前位元組。
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    /// 略過空白。
    fn skip_ws(&mut self) {
        self.pos = skip_ws(self.text, self.pos);
    }

    /// 解析一個值。
    fn parse_value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.nested(Self::parse_object),
            b'[' => self.nested(Self::parse_array),
            b'"' | b'\'' => self.parse_string().map(Value::String),
            b't' if self.starts_with("true") => {
                self.pos += 4;
                Some(Value::Bool(true))
            }
            b'f' if self.starts_with("false") => {
                self.pos += 5;
                Some(Value::Bool(false))
            }
            // `null`、`None`、`undefined` 一律視為空值。
            b'n' if self.starts_with("null") => {
                self.pos += 4;
                Some(Value::Null)
            }
            b'N' if self.starts_with("None") => {
                self.pos += 4;
                Some(Value::Null)
            }
            b'u' if self.starts_with("undefined") => {
                self.pos += 9;
                Some(Value::Null)
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            // 其他裸識別字（例如列舉值 `Student`）以字串解讀，保持寬容。
            _ => {
                let start = self.pos;
                while self.peek().is_some_and(is_ident_byte) {
                    self.pos += 1;
                }
                (self.pos > start).then(|| Value::String(self.text[start..self.pos].to_owned()))
            }
        }
    }

    /// 解析巢狀容器（物件或陣列）：深度超限即回 `None`，離開時一定還原深度。
    fn nested(&mut self, parse: impl FnOnce(&mut Self) -> Option<Value>) -> Option<Value> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let value = parse(self);
        self.depth -= 1;
        value
    }

    /// 解析物件（允許未加引號的鍵與尾逗號）。
    fn parse_object(&mut self) -> Option<Value> {
        self.pos += 1; // 目前指向 '{'
        let mut map = Map::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Some(Value::Object(map));
            }
            let key = self.parse_key()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Some(Value::Object(map));
                }
                _ => return None,
            }
        }
    }

    /// 解析陣列（允許尾逗號）。
    fn parse_array(&mut self) -> Option<Value> {
        self.pos += 1; // 目前指向 '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Some(Value::Array(items));
            }
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Some(Value::Array(items));
                }
                _ => return None,
            }
        }
    }

    /// 解析鍵名（字串或未加引號的識別字）。
    fn parse_key(&mut self) -> Option<String> {
        self.skip_ws();
        match self.peek()? {
            b'"' | b'\'' => self.parse_string(),
            _ => {
                let start = self.pos;
                while self.peek().is_some_and(is_ident_byte) {
                    self.pos += 1;
                }
                (self.pos > start).then(|| self.text[start..self.pos].to_owned())
            }
        }
    }

    /// 解析字串（含跳脫序列）。
    ///
    /// 跳脫字元一律以**完整字元**前進：`\` 之後可能是多位元組字元
    /// （例如 `"\中"` 的 JS identity escape）。若只前進一個位元組，
    /// `pos` 會落在字元中間，後續切片即 panic。
    fn parse_string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.pos += 1;
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            if byte == quote {
                self.pos += 1;
                return Some(out);
            }
            if byte == b'\\' {
                self.pos += 1;
                let escaped = self.text[self.pos..].chars().next()?;
                self.pos += escaped.len_utf8();
                match escaped {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => out.push(self.parse_unicode_escape()?),
                    // JS identity escape：`\中` 等同 `中`。
                    other => out.push(other),
                }
                continue;
            }
            let character = self.text[self.pos..].chars().next()?;
            out.push(character);
            self.pos += character.len_utf8();
        }
        None
    }

    /// 解析 `\uXXXX`（或 ES6 的 `\u{XXXXXX}`）跳脫（呼叫時已消費 `u`）。
    ///
    /// JS 與 JSON 都以 UTF-16 表示字串，非 BMP 字元（emoji、罕用漢字…）是一對
    /// 代理：`\uD83D\uDE00`。單獨看 `\uD83D` 時 `char::from_u32` 回 `None`，若
    /// 就此讓整段解析失敗，頁面裡任何一個這類字元（例如使用者暱稱）都會連帶讓
    /// `globalData` 讀不出來——`user_id` 取不到，個人作業全部退回「待核实」。
    /// 因此這裡必須自己組合代理對。
    ///
    /// 孤立代理（另一半缺失）對應不到合法字元，以 U+FFFD 取代：它是「原字元
    /// 不明」的標準表示，只損失一個字元，不該讓整個頁面解析失敗。語法本身錯誤
    /// （`\uZZZZ`、未閉合的 `\u{…`）仍回 `None`——那代表解析器與頁面格式不符，
    /// 應該看得見。
    fn parse_unicode_escape(&mut self) -> Option<char> {
        if self.peek() == Some(b'{') {
            return self.parse_code_point_escape();
        }
        let first = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&first) {
            // 高位代理：配對緊接其後的低位代理 `\uDC00`–`\uDFFF`。
            return match self.parse_low_surrogate() {
                Some(low) => char::from_u32(0x1_0000 + ((first - 0xD800) << 10) + (low - 0xDC00)),
                None => Some(REPLACEMENT),
            };
        }
        if (0xDC00..=0xDFFF).contains(&first) {
            // 低位代理在前：沒有可配對的高位。
            return Some(REPLACEMENT);
        }
        char::from_u32(first)
    }

    /// 解析 ES6 的碼位跳脫 `{XXXXXX}`（呼叫時位於 `{`）。
    fn parse_code_point_escape(&mut self) -> Option<char> {
        self.pos += 1;
        let start = self.pos;
        while self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        let hex = &self.text[start..self.pos];
        if hex.is_empty() || hex.len() > 6 || self.peek() != Some(b'}') {
            return None;
        }
        self.pos += 1;
        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
    }

    /// 讀取 4 位十六進位（呼叫時位於第一位）；不是 4 位十六進位時回 `None`
    /// 且不消費任何內容。
    fn parse_hex4(&mut self) -> Option<u32> {
        let hex = self.text.get(self.pos..self.pos + 4)?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        self.pos += 4;
        u32::from_str_radix(hex, 16).ok()
    }

    /// 讀取緊接其後的 `\uXXXX` 低位代理；不是時回 `None` 且不消費任何內容。
    fn parse_low_surrogate(&mut self) -> Option<u32> {
        let hex = self.text[self.pos..].strip_prefix("\\u")?.get(..4)?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        if !(0xDC00..=0xDFFF).contains(&value) {
            return None;
        }
        self.pos += 6;
        Some(value)
    }

    /// 解析數字（整數優先，超出範圍退為浮點）。
    fn parse_number(&mut self) -> Option<Value> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = &self.text[start..self.pos];
        if let Ok(integer) = text.parse::<i64>() {
            return Some(Value::Number(Number::from(integer)));
        }
        Number::from_f64(text.parse::<f64>().ok()?).map(Value::Number)
    }

    /// 目前位置是否以 `needle` 開頭。
    fn starts_with(&self, needle: &str) -> bool {
        self.text[self.pos..].starts_with(needle)
    }
}

#[cfg(test)]
#[path = "tests/js_object_test.rs"]
mod js_object_test;
