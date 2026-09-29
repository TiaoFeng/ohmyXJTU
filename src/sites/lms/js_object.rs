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
}

impl<'a> Parser<'a> {
    /// 建立解析器。
    fn new(text: &'a str, pos: usize) -> Self {
        Self { text, pos }
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
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
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
                let escaped = self.peek()?;
                self.pos += 1;
                match escaped {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => out.push(self.parse_unicode_escape()?),
                    other => out.push(char::from(other)),
                }
                continue;
            }
            let character = self.text[self.pos..].chars().next()?;
            out.push(character);
            self.pos += character.len_utf8();
        }
        None
    }

    /// 解析 `\uXXXX` 的跳脫（呼叫時已消費 `u`）。
    fn parse_unicode_escape(&mut self) -> Option<char> {
        let end = self.pos + 4;
        let hex = self.text.get(self.pos..end)?;
        self.pos = end;
        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
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
