//! 以 `reqwest` 實作的 HTTP 客戶端。

use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use url::Url;

use super::{Body, HttpClient, HttpRequest, HttpResponse, Method};
use crate::error::{AppError, AppResult, NetworkKind};
use crate::webvpn;

/// 單次請求的預設逾時（含連線、傳輸與讀取整段）。
///
/// 實測校內服務的單次往返為 0.3～3.2 秒，15 秒已相當寬裕；縮短逾時讓
/// 「連不上、逾時」更快地暴露出來，使用者不必枯等半分鐘才看到失敗
///（失敗後會自動重試，見 `crate::task::worker`）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// 建立連線（含 DNS 解析與 TLS 握手）的逾時。
///
/// 離線或目標不可達時，若只靠總逾時，使用者要等滿 15 秒；連線階段單獨
/// 設 10 秒可讓這類失敗更早結束。正常請求的連線階段遠快於此值。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 最多跟隨的重定向次數。
const MAX_REDIRECTS: usize = 10;

/// 單一請求逾時預算的上限。
///
/// 防禦用：`Instant + Duration` 溢位會直接 panic，而預算來自呼叫端可指定的
/// `HttpRequest::timeout`。實際呼叫端只傳 15 秒（預設）與 5 秒（探測），這個
/// 上限只是保證「不會有人傳進一個能讓行程崩潰的值」。
const MAX_REQUEST_BUDGET: Duration = Duration::from_secs(600);

/// 錯誤鏈摘要的最大長度（字元數）。
const MAX_DETAIL_CHARS: usize = 320;

/// 跨來源重定向時仍可保留的標頭。
///
/// 採白名單而非黑名單：日後新增任何自訂標頭（例如新的業務憑證）都會預設在
/// 跨來源時被丟棄，不會因為忘記更新清單而外洩。Cookie 由 cookie jar 依網域
/// 自行處理，不需要（也不應該）由呼叫端轉送。
const CROSS_ORIGIN_SAFE_HEADERS: [&str; 3] = ["user-agent", "accept", "accept-language"];

/// 內建 cookie jar 的阻塞式 HTTP 客戶端。
///
/// 同一個實例共享連線池與 cookie，對應一個「會話後端」。
///
/// 重定向一律由本層**逐跳**處理（客戶端本身設為不跟隨）：`reqwest` 的自動跟隨
/// 只限制跳數，跨主機時不會移除自訂標頭（例如考勤的 `X-Business-Token`）、
/// 不阻止 HTTPS→HTTP 降級，也會讓 307/308 把 POST 主體重送到別的來源。
/// 需要觀察 302 等狀態碼時，仍可用 [`HttpRequest::no_redirect`] 取得原始回應。
#[derive(Debug, Clone)]
pub struct ReqwestClient {
    client: Client,
    /// 重定向目的主機的信任判斷（測試可放寬為本機假伺服器）。
    trusted_host: fn(&str) -> bool,
    /// 未指定逾時時使用的預設值。
    ///
    /// 與套用在客戶端上的逾時是同一個值（見 [`build_client`]）：重定向鏈的
    /// 預算必須以它為基準，否則「每跳各算一次預設逾時」的最壞情形又會回來。
    default_timeout: Duration,
}

impl ReqwestClient {
    /// 建立客戶端。
    pub fn new(user_agent: impl Into<String>) -> AppResult<Self> {
        Self::with_trusted_host(user_agent, is_trusted_redirect_host, DEFAULT_TIMEOUT)
    }

    /// 建立客戶端並指定重定向目的主機的信任判斷（測試用：本機 TCP 假伺服器）。
    #[cfg(test)]
    pub fn new_insecure_for_tests(user_agent: impl Into<String>) -> AppResult<Self> {
        Self::with_trusted_host(user_agent, |_| true, DEFAULT_TIMEOUT)
    }

    /// 建立客戶端並指定預設逾時（測試用：讓「未指定逾時」的情形也能在毫秒級驗證）。
    #[cfg(test)]
    pub fn new_insecure_with_timeout_for_tests(
        user_agent: impl Into<String>,
        default_timeout: Duration,
    ) -> AppResult<Self> {
        Self::with_trusted_host(user_agent, |_| true, default_timeout)
    }

    fn with_trusted_host(
        user_agent: impl Into<String>,
        trusted_host: fn(&str) -> bool,
        default_timeout: Duration,
    ) -> AppResult<Self> {
        let user_agent = user_agent.into();
        Ok(Self {
            client: build_client(&user_agent, default_timeout)?,
            trusted_host,
            default_timeout,
        })
    }

    /// 送出請求，並依需求逐跳跟隨重定向。
    fn send_once(&self, request: &HttpRequest) -> AppResult<HttpResponse> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
            Method::Put => self.client.put(&request.url),
            Method::Delete => self.client.delete(&request.url),
            Method::Head => self.client.head(&request.url),
            Method::Mkcol => self.client.request(mkcol_method(), &request.url),
        };

        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        builder = match &request.body {
            Some(Body::Form(fields)) => builder.form(fields),
            Some(Body::Json(value)) => builder.json(value),
            Some(Body::Bytes(bytes)) => builder.body(bytes.clone()),
            None => builder,
        };

        let response = builder.send().map_err(map_error)?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    // 以 lossy 轉換保留非 UTF-8 標頭的存在與近似值：`unwrap_or_default`
                    // 會把任何非 UTF-8 標頭靜默變成空字串（例如 `content-type`），
                    // 使登入態失效判定失去依據。
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().map_err(map_error)?.to_vec();

        Ok(HttpResponse {
            status,
            final_url,
            headers,
            body,
        })
    }

    /// 逐跳送出請求，直到取得最終回應。
    ///
    /// 每一跳都重新檢查目的地（協定、主機）、是否同源，以及下一個請求要帶
    /// 哪些標頭與主體；跨來源時只保留 [`CROSS_ORIGIN_SAFE_HEADERS`]，並拒絕
    /// 讓帶主體的 307/308 重送到其他來源（等同洩漏帳密、簡訊碼或業務憑證）。
    fn send_following(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        let mut method = request.method;
        let mut body = request.body.clone();
        let mut headers = request.headers.clone();
        let mut url = request.url.clone();
        let mut hops = 0_usize;
        // 逾時是**整條請求**的預算，不是每一跳各算一次：逐跳各自吃滿逾時的話，
        // 一條十跳的慢速重導鏈最壞會拖上「逾時 × 10」，而工作執行緒在這段期間
        // 無法處理任何控制任務（取消登入、結束、設定），使用者看到的就是卡死。
        //
        // 基準必須是「請求逾時或**客戶端預設逾時**」：只有探測請求會顯式指定逾時
        //（而且它不跟隨重定向），其餘請求一律為 `None`——只認 `request.timeout`
        // 的話，這道預算在生產路徑根本不會成立。
        let budget = request
            .timeout
            .unwrap_or(self.default_timeout)
            .min(MAX_REQUEST_BUDGET);
        let deadline = Instant::now() + budget;

        loop {
            if Instant::now() >= deadline {
                return Err(AppError::network_kind(
                    NetworkKind::Timeout,
                    "请求超时（重定向链未在时限内完成）",
                ));
            }
            let response = self.send_once(&HttpRequest {
                method,
                url: url.clone(),
                headers: headers.clone(),
                body: body.clone(),
                timeout: Some(remaining_timeout(deadline)),
                follow_redirects: false,
            })?;

            let Some(location) = redirect_location(&response) else {
                return Ok(response);
            };
            hops += 1;
            let previous = Url::parse(&response.final_url)
                .map_err(|_| redirect_error("重定向来源无法解析"))?;
            let plan = plan_redirect(
                &previous,
                &location,
                response.status,
                method,
                body.is_some(),
                hops,
                self.trusted_host,
            )?;

            if !plan.keep_headers {
                headers.retain(|(name, _)| is_cross_origin_safe(name));
            }
            if !plan.keep_body {
                body = None;
            }
            method = plan.method;
            url = plan.url;
        }
    }
}

impl HttpClient for ReqwestClient {
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        if request.follow_redirects {
            self.send_following(request)
        } else {
            self.send_once(&request)
        }
    }
}

/// 重定向目的主機是否可接受。
fn is_trusted_redirect_host(host: &str) -> bool {
    webvpn::is_school_host(host) || host.eq_ignore_ascii_case(webvpn::WEBVPN_HOST)
}

/// 下一跳還剩多少逾時預算。
///
/// 呼叫端已先確認「還沒到截止時間」，因此回傳值必定大於零；`saturating_*`
/// 只是防禦（時鐘不會倒退，但沒必要讓這裡成為 panic 的來源）。
fn remaining_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// 跨來源重定向時可保留的標頭。
fn is_cross_origin_safe(name: &str) -> bool {
    CROSS_ORIGIN_SAFE_HEADERS
        .iter()
        .any(|safe| name.eq_ignore_ascii_case(safe))
}

/// 回應是否要求重定向，以及目標位置。
///
/// 沒有 `Location` 的 3xx 不跟隨：原樣回傳，由呼叫端判定（例如登入態失效）。
fn redirect_location(response: &HttpResponse) -> Option<String> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response.header("location")?.trim();
    (!location.is_empty()).then(|| location.to_owned())
}

/// 一次重定向的處置。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RedirectPlan {
    /// 下一跳的網址。
    url: String,
    /// 下一跳使用的方法。
    method: Method,
    /// 是否保留原本的請求主體。
    keep_body: bool,
    /// 是否保留自訂標頭（僅同源時保留）。
    keep_headers: bool,
}

/// 決定下一跳要怎麼送（純函式，便於測試）。
///
/// `trusted` 為目的主機的信任判斷；`hops` 為本次已跟隨的跳數（含這一跳）。
fn plan_redirect(
    previous: &Url,
    location: &str,
    status: u16,
    method: Method,
    has_body: bool,
    hops: usize,
    trusted: fn(&str) -> bool,
) -> AppResult<RedirectPlan> {
    if hops > MAX_REDIRECTS {
        return Err(redirect_error("重定向次数过多"));
    }
    let next = previous
        .join(location)
        .map_err(|_| redirect_error("重定向地址无法解析"))?;
    match next.scheme() {
        "https" => {}
        "http" => {
            // 學校端點全部是 HTTPS；從 HTTPS 降到 HTTP 一律拒絕。
            if previous.scheme() == "https" {
                return Err(redirect_error("重定向试图降级为不加密连接"));
            }
        }
        _ => return Err(redirect_error("重定向到不支持的协议")),
    }
    let host = next
        .host_str()
        .ok_or_else(|| redirect_error("重定向地址缺少主机名"))?;
    if !trusted(host) {
        return Err(redirect_error("重定向到学校网域之外的主机（已中止）"));
    }
    // WebVPN 代理網址的外層主機都是閘道，真正的目標藏在路徑裡：只檢查外層
    // 會讓「代理到 http:// 或校外主機」的重導通過，查詢參數（可能含 ticket）
    // 也會被一起轉送。內層目標必須是 HTTPS 的學校主機，且必須解得開。
    if host.eq_ignore_ascii_case(webvpn::WEBVPN_HOST)
        && let Some(target) = webvpn::proxied_target(next.path())
    {
        if target.scheme != "https" {
            return Err(redirect_error("WebVPN 重定向的代理目标不是 HTTPS"));
        }
        let inner = target
            .host
            .ok_or_else(|| redirect_error("WebVPN 重定向的代理目标无法解析"))?;
        if !webvpn::is_school_host(&inner) {
            return Err(redirect_error("WebVPN 重定向的代理目标位于学校网域之外"));
        }
    }

    let same_origin = same_origin(previous, &next);
    // 307/308 會保留方法與主體：跨來源重送等同把帳密、簡訊碼或業務憑證
    // 送到別的主機，直接拒絕。
    let keeps_method = matches!(status, 307 | 308);
    let resend_body = keeps_method && has_body;
    if resend_body && !same_origin {
        return Err(redirect_error("拒绝把请求主体重送到其他来源"));
    }

    Ok(RedirectPlan {
        url: next.to_string(),
        method: redirect_method(status, method),
        keep_body: resend_body,
        keep_headers: same_origin,
    })
}

/// 依狀態碼決定下一跳的方法。
///
/// 307/308 保留原方法（與主體，見上）；301/302/303 對 HEAD 保留 HEAD，否則
/// HEAD 會變成下載主體；其餘（含 POST／PUT／DELETE 的一般重導）一律改用 GET。
fn redirect_method(status: u16, method: Method) -> Method {
    if matches!(status, 307 | 308) {
        return method;
    }
    match method {
        Method::Get | Method::Head => method,
        Method::Post | Method::Put | Method::Delete | Method::Mkcol => Method::Get,
    }
}

/// WebDAV 的 `MKCOL` 方法（`reqwest` 沒有提供對應常數）。
fn mkcol_method() -> reqwest::Method {
    reqwest::Method::from_bytes(b"MKCOL").expect("MKCOL 是合法的方法名")
}

/// 網址的可比較來源（協定 ＋ 主機 ＋ 有效埠）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

/// 兩個網址是否同源。
///
/// WebVPN 代理網址以**代理目標**判定（外層主機都是 `webvpn.xjtu.edu.cn`，
/// 不代表內層目的站點相同）；無法判定時一律視為跨來源。
fn same_origin(previous: &Url, next: &Url) -> bool {
    match (origin_of(previous), origin_of(next)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// 取網址的來源；WebVPN 代理網址以代理目標為準，無法判定時回 `None`。
fn origin_of(url: &Url) -> Option<Origin> {
    let host = url.host_str()?;
    if host.eq_ignore_ascii_case(webvpn::WEBVPN_HOST)
        && let Some(target) = webvpn::proxied_target(url.path())
    {
        let inner = target.host?;
        let port = target.port.or_else(|| known_port(&target.scheme));
        return Some(Origin {
            scheme: target.scheme,
            host: inner.to_ascii_lowercase(),
            port,
        });
    }
    Some(Origin {
        scheme: url.scheme().to_owned(),
        host: host.to_ascii_lowercase(),
        port: url.port_or_known_default(),
    })
}

/// 協定的預設埠。
fn known_port(scheme: &str) -> Option<u16> {
    match scheme {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    }
}

/// 建立重定向相關的網路錯誤（訊息不含完整網址與查詢參數）。
fn redirect_error(detail: &str) -> AppError {
    AppError::network_kind(NetworkKind::Redirect, detail.to_owned())
}

fn build_client(user_agent: &str, timeout: Duration) -> AppResult<Client> {
    Client::builder()
        .user_agent(user_agent)
        .cookie_store(true)
        // 重定向一律由本層逐跳處理（見 `ReqwestClient::send_following`）。
        .redirect(Policy::none())
        .timeout(timeout)
        // 連線階段（DNS、TCP、TLS）另有較短的逾時：目標不可達時能更快回報，
        // 不必等滿總逾時（請求可在 [`HttpRequest::timeout`] 個別覆寫總逾時，
        // 但連線逾時一律以此為上限）。
        .connect_timeout(CONNECT_TIMEOUT)
        // 考勤入口（bk-kq.xjtu.edu.cn）的第一個回應以舊式多行標頭承載
        // Content-Security-Policy（續行使用裸 LF）。Hyper 預設拒收這類標頭，
        // 會讓請求在還沒開始重定向前就失敗；Python requests 對此寬容，參考實作
        // 因此不受影響。這裡只放寬這一種舊式折行格式，不選擇「忽略所有無效標頭」，
        // TLS 驗證等安全性設定維持不變。
        .http1_allow_obsolete_multiline_headers_in_responses(true)
        .build()
        .map_err(|err| AppError::network(format!("初始化 HTTP 客户端失败：{err}")))
}

/// 將 `reqwest` 錯誤映射為帶類別的網路錯誤。
///
/// 錯誤鏈會保留底層原因（例如 `invalid HTTP header parsed`），但一律去除
/// URL 與查詢參數，避免把敏感資訊帶進使用者可見訊息。
fn map_error(err: reqwest::Error) -> AppError {
    AppError::network_kind(classify(&err), describe(&err))
}

/// 依錯誤鏈判斷網路錯誤類別。
fn classify(err: &reqwest::Error) -> NetworkKind {
    classify_chain(
        &chain_text(err),
        err.is_timeout(),
        err.is_redirect(),
        err.is_connect(),
    )
}

/// 分類邏輯本體（純函式，便於以真實錯誤鏈文字測試）。
///
/// `chain` 需為小寫的錯誤鏈全文；旗標對應 `reqwest::Error` 的
/// `is_timeout`／`is_redirect`／`is_connect`。
fn classify_chain(
    chain: &str,
    is_timeout: bool,
    is_redirect: bool,
    is_connect: bool,
) -> NetworkKind {
    if is_timeout {
        return NetworkKind::Timeout;
    }
    if is_redirect {
        return NetworkKind::Redirect;
    }
    if chain.contains("invalid http header") || chain.contains("invalid header") {
        return NetworkKind::HttpParse;
    }
    if chain.contains("dns")
        || chain.contains("failed to lookup")
        || chain.contains("name or service not known")
        || chain.contains("nodename nor servname")
    {
        return NetworkKind::Dns;
    }
    if chain.contains("certificate") || chain.contains("tls") || chain.contains("handshake") {
        return NetworkKind::Tls;
    }
    if is_connect
        || chain.contains("connection closed")
        || chain.contains("connection reset")
        || chain.contains("broken pipe")
        || chain.contains("unexpected eof")
    {
        return NetworkKind::Connect;
    }
    NetworkKind::Other
}

/// 串接錯誤鏈全文（小寫）供關鍵字判類。
fn chain_text(err: &reqwest::Error) -> String {
    let mut text = String::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(item) = current {
        text.push_str(&item.to_string().to_lowercase());
        text.push('\n');
        current = item.source();
    }
    text
}

/// 將錯誤鏈整理為去識別化的單行摘要。
fn describe(err: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(item) = current {
        let text = sanitize(&item.to_string());
        if !text.is_empty() && parts.last().map(String::as_str) != Some(text.as_str()) {
            parts.push(text);
        }
        current = item.source();
    }

    let mut detail = parts.join("：");
    if detail.chars().count() > MAX_DETAIL_CHARS {
        let cut = detail
            .char_indices()
            .nth(MAX_DETAIL_CHARS)
            .map_or(detail.len(), |(index, _)| index);
        detail.truncate(cut);
        detail.push('…');
    }
    detail
}

/// 以 `<url>` 取代任何含查詢參數的網址。
fn sanitize(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            if token.contains("://") {
                "<url>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[path = "tests/reqwest_client_test.rs"]
mod reqwest_client_test;
