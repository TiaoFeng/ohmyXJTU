//! 統一身份認證的登入驅動器。
//!
//! 移植參考實作 `NewLogin` 的狀態機設計：`advance` 是「驅動器」，
//! 每次呼叫都會依當下內部狀態執行對應動作，並回報下一個需要的步驟。
//!
//! 呼叫端（背景 worker）依序處理：
//!
//! 1. [`LoginDriver::start`]：首次呼叫，帶入帳密。
//! 2. [`LoginDriver::submit_captcha`]：需要圖片驗證碼時。
//! 3. [`LoginDriver::resume`]：完成簡訊驗證或身份選擇後繼續。
//!
//! 每個步驟的結果都是 [`LoginReply`]，直到 `Success` 或 `Fail` 為止。

use std::path::PathBuf;
use std::sync::Arc;

use ::rsa::RsaPublicKey;
use serde_json::json;
use url::Url;

use crate::credentials::Credentials;
use crate::error::{AppError, AppResult};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::json::split_envelope;

use super::html;
use super::rsa;
use super::state::{AccountType, LoginReply, MfaFlow};
use super::{captcha, webvpn};

/// 統一認證入口主機。
pub const LOGIN_HOST: &str = "https://login.xjtu.edu.cn";

/// MFA 偵測端點。
const MFA_DETECT_URL: &str = "https://login.xjtu.edu.cn/cas/mfa/detect";
/// 發送簡訊驗證碼的端點。
const MFA_SEND_URL: &str = "https://login.xjtu.edu.cn/attest/api/guard/securephone/send";
/// 核對簡訊驗證碼的端點。
const MFA_VALID_URL: &str = "https://login.xjtu.edu.cn/attest/api/guard/securephone/valid";
/// 提交身份選擇的端點。
const ACCOUNT_CHOICE_URL: &str = "https://login.xjtu.edu.cn/cas/login";

/// 連續失敗達此次數後，伺服器會要求輸入圖片驗證碼。
const CAPTCHA_THRESHOLD: u32 = 3;
/// 核對簡訊驗證碼的成功狀態碼（伺服器可能以整數 `2` 或字串 `"2"` 傳送）。
const MFA_SUCCESS_CODE: i64 = 2;

/// 簡訊驗證上下文。
#[derive(Debug, Clone)]
struct MfaContext {
    flow: MfaFlow,
    state: String,
    required: bool,
    gid: Option<String>,
    phone: Option<String>,
}

/// Safety Verify 二次認證的待處理狀態。
#[derive(Debug, Clone)]
struct SafetyVerifyState {
    response: HttpResponse,
}

/// 登入驅動器。
pub struct LoginDriver {
    client: Arc<dyn HttpClient>,
    post_url: String,
    execution: Option<String>,
    already_authenticated: Option<HttpResponse>,
    visitor_id: String,
    mfa_enabled: bool,
    fail_count: u32,
    /// 已解析的伺服器公鑰（快取；解析成功才會寫入）。
    rsa_public_key: Option<RsaPublicKey>,
    has_login: bool,
    account_type: AccountType,
    choose_account_response: Option<HttpResponse>,
    safety_verify: Option<SafetyVerifyState>,
    /// 二次認證已進入簡訊驗證階段（`safety_verify` 的頁面待提交）。
    ///
    /// 用於區分「要求簡訊驗證」與「提交隱藏表單」兩個階段（對齊參考實作的
    /// `_safety_verify_mfa_requested`）：初始頁即二次認證時，第一次 `advance`
    /// 只要求驗證，使用者完成後才提交頁面表單。
    safety_verify_mfa_requested: bool,
    mfa: Option<MfaContext>,
    username: Option<String>,
    encrypted_password: Option<String>,
    captcha_code: String,
    /// 最近一次提交是否帶了圖片驗證碼（用於判斷失敗可否重試）。
    captcha_submitted: bool,
    final_response: Option<HttpResponse>,
    /// 本次登入是否直接沿用伺服器上既有的登入態（未提交帳密即完成）。
    used_existing_session: bool,
}

impl LoginDriver {
    /// 建立驅動器：先請求登入網址，決定提交位置並判斷是否已具備登入態。
    pub fn new(client: Arc<dyn HttpClient>, login_url: &str, visitor_id: &str) -> AppResult<Self> {
        let response = client.send(HttpRequest::get(login_url))?;
        if response.status >= 400 {
            return Err(AppError::Http {
                status: response.status,
            });
        }
        // 登入表單只允許提交給學校網域的主機：重定向若離開學校網域，
        // 直接中止，避免把帳密（密文）送到非學校主機。
        ensure_trusted_submit_target(&response.final_url)?;

        let text = response.text();
        let execution = html::execution_value(&text);
        let initial_safety_verify = html::is_safety_verify_page(&text);
        let already_authenticated = if !initial_safety_verify
            && execution.is_none()
            && !response.final_url.contains("/cas/login")
        {
            Some(response.clone())
        } else {
            None
        };
        let mfa_enabled = html::mfa_enabled(&text);
        let post_url = response.final_url.clone();
        // 初始頁即二次認證：保存頁面，供後續以 `secState` 走安全驗證流程
        //（參考實作的 `_safety_verify_response`）。
        let safety_verify = initial_safety_verify.then_some(SafetyVerifyState { response });

        Ok(Self {
            client,
            post_url,
            execution,
            already_authenticated,
            visitor_id: visitor_id.to_owned(),
            mfa_enabled,
            fail_count: 0,
            rsa_public_key: None,
            has_login: false,
            account_type: AccountType::Undergraduate,
            choose_account_response: None,
            safety_verify,
            mfa: None,
            safety_verify_mfa_requested: false,
            username: None,
            encrypted_password: None,
            captcha_code: String::new(),
            captcha_submitted: false,
            final_response: None,
            used_existing_session: false,
        })
    }

    /// 使用的 HTTP 客戶端（供站點在登入成功後換取業務 token）。
    pub fn client(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.client)
    }

    /// 登入成功時的最終回應。
    pub fn final_response(&self) -> Option<&HttpResponse> {
        self.final_response.as_ref()
    }

    /// 登入網址是否已具備登入態（不需要提交帳密）。
    pub fn is_already_authenticated(&self) -> bool {
        self.already_authenticated.is_some()
    }

    /// 本次登入是否直接沿用既有登入態（未向伺服器提交帳密）。
    ///
    /// 除了登入網址已具備登入態，初始頁即二次認證（沿用伺服器既有會話、
    /// 以簡訊驗證完成）也算：換帳號時若為真，代表新憑證從未被伺服器
    /// 驗證過，不得寫回保險庫。
    pub fn used_existing_session(&self) -> bool {
        self.used_existing_session
    }

    /// 最近一次提交是否帶了圖片驗證碼。
    ///
    /// 帶驗證碼而失敗多半是驗證碼本身填錯：使用者重輸即可，不要當成
    /// 帳密錯誤而作廢整次登入。
    pub fn last_attempt_submitted_captcha(&self) -> bool {
        self.captcha_submitted
    }

    /// 本次登入已連續失敗的次數（伺服器以此決定是否要求圖片驗證碼）。
    pub fn fail_count(&self) -> u32 {
        self.fail_count
    }

    /// 設定本次登入的起始失敗次數。
    ///
    /// 驗證碼門檻是以「同一帳號連續失敗」計算，但驅動器每次重試都會重建；
    /// 由呼叫端保存次數並在重建後注入，門檻才達得到（否則每次重試都从 0 開始）。
    pub fn set_fail_count(&mut self, count: u32) {
        self.fail_count = count;
    }

    /// 首次登入：帶入帳號密碼。
    pub fn start(
        &mut self,
        credentials: &Credentials,
        account_type: AccountType,
    ) -> AppResult<LoginReply> {
        self.account_type = account_type;
        self.username = Some(credentials.username.clone());
        // 已具備登入態、或初始頁即二次認證（本次登入不會提交帳密）時，
        // 都不需要抓取公鑰。
        if self.already_authenticated.is_none() && self.safety_verify.is_none() {
            let public_key = self.public_key()?;
            self.encrypted_password =
                Some(rsa::encrypt_password(&credentials.password, &public_key)?);
        }
        self.captcha_code.clear();
        self.advance()
    }

    /// 以圖片驗證碼繼續登入。
    pub fn submit_captcha(&mut self, code: &str) -> AppResult<LoginReply> {
        self.captcha_code = code.to_owned();
        self.advance()
    }

    /// 在完成簡訊驗證或身份選擇後繼續登入。
    pub fn resume(&mut self) -> AppResult<LoginReply> {
        self.advance()
    }

    /// 取得驗證碼圖片並寫入暫存檔。
    pub fn fetch_captcha(&self) -> AppResult<PathBuf> {
        captcha::fetch(self.client.as_ref())
    }

    /// 取得綁定手機號（中間四位由伺服器遮蔽）。
    pub fn mfa_phone(&mut self) -> AppResult<String> {
        if let Some(phone) = self.mfa.as_ref().and_then(|ctx| ctx.phone.clone()) {
            return Ok(phone);
        }

        let context = self
            .mfa
            .as_ref()
            .ok_or_else(|| AppError::protocol("当前不需要短信验证"))?;
        let flow = context.flow;
        let state = context.state.clone();
        let mut url = Url::parse(&format!(
            "{LOGIN_HOST}/cas/{}/initByType/securephone",
            flow.path_segment()
        ))
        .map_err(|err| AppError::protocol(format!("无法构造手机号查询地址：{err}")))?;
        url.query_pairs_mut().append_pair("state", &state);

        let response = self.client.send(HttpRequest::get(url.to_string()))?;
        let data = split_envelope(&response, "手机号查询")?;
        let phone = data
            .get("securePhone")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::protocol("绑定手机信息缺少 securePhone 字段"))?
            .to_owned();
        let gid = data
            .get("gid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        if let Some(context) = self.mfa.as_mut() {
            context.gid = gid;
            context.phone = Some(phone.clone());
        }
        Ok(phone)
    }

    /// 發送簡訊驗證碼，回傳（遮蔽後的）手機號。
    pub fn send_mfa_code(&mut self) -> AppResult<String> {
        let phone = self.mfa_phone()?;
        let gid = self.mfa_gid()?;
        let response = self
            .client
            .send(HttpRequest::post_json(MFA_SEND_URL, json!({ "gid": gid })))?;
        split_envelope(&response, "发送短信验证码")?;
        Ok(phone)
    }

    /// 核對簡訊驗證碼。
    pub fn verify_mfa_code(&mut self, code: &str) -> AppResult<()> {
        let gid = self.mfa_gid()?;
        let response = self.client.send(HttpRequest::post_json(
            MFA_VALID_URL,
            json!({ "gid": gid, "code": code }),
        ))?;
        let data = split_envelope(&response, "核验短信验证码")?;
        let passed = match data.get("status") {
            // 缺少狀態欄位（或為 null）時視為通過——與參考實作一致。
            None | Some(serde_json::Value::Null) => true,
            // 其餘只接受成功碼：整數 2 或可解析為 2 的數值字串。
            Some(status) => {
                status.as_i64() == Some(MFA_SUCCESS_CODE)
                    || status.as_str().and_then(|text| text.parse::<i64>().ok())
                        == Some(MFA_SUCCESS_CODE)
            }
        };
        if !passed {
            // 驗證碼填錯是可重試的：使用者重輸即可，不應作廢整次登入。
            return Err(AppError::VerificationRetry(
                "短信验证码不正确，请重试".to_owned(),
            ));
        }
        Ok(())
    }

    /// 登入狀態機的驅動器。
    fn advance(&mut self) -> AppResult<LoginReply> {
        self.captcha_submitted = false;
        if let Some(response) = self.already_authenticated.take() {
            self.has_login = true;
            self.used_existing_session = true;
            self.final_response = Some(response);
            return Ok(LoginReply::Success);
        }

        if self.choose_account_response.is_some() {
            return self.finish_account_choice();
        }

        if self.safety_verify.is_some() {
            // 兩階段（對齊參考實作）：先要求簡訊驗證，使用者完成後
            //（`resume`）才提交頁面的隱藏表單。
            if !self.safety_verify_mfa_requested {
                // 尚未要求過驗證代表這是「初始頁即二次認證」：本次登入沿用
                // 伺服器既有會話完成、從未提交帳密，換帳號時不得寫回憑證。
                self.used_existing_session = true;
                return self.require_safety_verify();
            }
            return self.finish_safety_verify();
        }

        if self.has_login {
            return Err(AppError::protocol("登录流程已经结束，不能重复提交"));
        }

        if self.fail_count >= CAPTCHA_THRESHOLD && self.captcha_code.is_empty() {
            return Ok(LoginReply::NeedCaptcha);
        }

        if let Some(reply) = self.detect_mfa()? {
            return Ok(reply);
        }

        let execution = self
            .execution
            .clone()
            .ok_or_else(|| AppError::protocol("登录页面缺少 execution 字段"))?;
        let username = self
            .username
            .clone()
            .ok_or_else(|| AppError::protocol("尚未提供用户名"))?;
        let password = self
            .encrypted_password
            .clone()
            .ok_or_else(|| AppError::protocol("尚未提供密码"))?;
        let mfa_state = self
            .mfa
            .as_ref()
            .map_or(String::new(), |ctx| ctx.state.clone());

        // 這一步真的送出帳密（可能帶驗證碼）：記下來供失敗時判斷可否重試。
        self.captcha_submitted = !self.captcha_code.is_empty();
        let response = self.post_form(
            &self.post_url.clone(),
            vec![
                ("username", username),
                ("password", password),
                ("execution", execution),
                ("_eventId", "submit".to_owned()),
                ("submit1", "Login1".to_owned()),
                ("fpVisitorId", self.visitor_id.clone()),
                ("captcha", self.captcha_code.clone()),
                ("currentMenu", "1".to_owned()),
                ("failN", self.fail_count.to_string()),
                ("mfaState", mfa_state),
                ("geolocation", String::new()),
                ("trustAgent", self.trust_agent()),
            ],
        )?;
        self.process_login_response(response)
    }

    /// 登入前的 MFA 偵測；回傳 `Some(NeedMfa)` 代表需要簡訊驗證。
    fn detect_mfa(&mut self) -> AppResult<Option<LoginReply>> {
        let should_detect = self.mfa_enabled
            && !self.has_login
            && self.mfa.as_ref().is_none_or(|ctx| !ctx.required);
        if !should_detect {
            return Ok(None);
        }

        let username = self
            .username
            .clone()
            .ok_or_else(|| AppError::protocol("尚未提供用户名"))?;
        let password = self
            .encrypted_password
            .clone()
            .ok_or_else(|| AppError::protocol("尚未提供密码"))?;

        let response = self.client.send(
            HttpRequest::post_form(
                MFA_DETECT_URL,
                vec![
                    ("username", username),
                    ("password", password),
                    ("fpVisitorId", self.visitor_id.clone()),
                    ("loginType", "passwordLogin".to_owned()),
                ],
            )
            .header("Referer", self.post_url.clone()),
        )?;
        let value: serde_json::Value = response.json()?;
        let state = value
            .pointer("/data/state")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::protocol("MFA 检测响应缺少 state 字段"))?
            .to_owned();
        let need = value
            .pointer("/data/need")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        self.mfa = Some(MfaContext {
            flow: MfaFlow::Detect,
            state,
            required: need,
            gid: None,
            phone: None,
        });

        Ok(need.then_some(LoginReply::NeedMfa))
    }

    fn process_login_response(&mut self, response: HttpResponse) -> AppResult<LoginReply> {
        let text = response.text();
        let alert = html::alert_message(&text);

        if response.status == 401 {
            self.fail_count += 1;
            self.captcha_code.clear();
            let message = alert.map_or_else(
                || "登录失败，用户名或密码错误。".to_owned(),
                |alert| alert.text(),
            );
            return Ok(LoginReply::Fail { message });
        }
        response.error_for_status()?;

        if let Some(alert) = alert {
            self.fail_count += 1;
            self.captcha_code.clear();
            return Ok(LoginReply::Fail {
                message: format!("登录失败：{}", alert.text()),
            });
        }

        if html::is_safety_verify_page(&text) {
            self.safety_verify = Some(SafetyVerifyState { response });
            return self.require_safety_verify();
        }

        self.fail_count = 0;
        if let Some(choices) = html::account_choices(&text) {
            self.choose_account_response = Some(response);
            return Ok(LoginReply::NeedAccountChoice(choices));
        }

        self.has_login = true;
        self.final_response = Some(response);
        Ok(LoginReply::Success)
    }

    /// 進入二次認證的簡訊驗證階段（頁面已在 `safety_verify` 中待提交）。
    ///
    /// 無論是提交帳密後才收到二次認證頁，或登入入口直接落在二次認證頁，
    /// 都在這裡切換到安全驗證流程；使用者完成簡訊驗證後由
    /// [`Self::finish_safety_verify`] 提交頁面的隱藏表單。
    fn require_safety_verify(&mut self) -> AppResult<LoginReply> {
        let state = self
            .safety_verify
            .as_ref()
            .ok_or_else(|| AppError::protocol("当前不需要完成二次认证"))?;
        let sec_state = html::input_value(&state.response.text(), "secState")
            .ok_or_else(|| AppError::protocol("二次认证页面缺少 secState 字段"))?;

        self.fail_count = 0;
        self.safety_verify_mfa_requested = true;
        self.mfa = Some(MfaContext {
            flow: MfaFlow::SafetyVerify,
            state: sec_state,
            required: true,
            gid: None,
            phone: None,
        });
        Ok(LoginReply::NeedMfa)
    }

    fn finish_safety_verify(&mut self) -> AppResult<LoginReply> {
        let state = self
            .safety_verify
            .take()
            .ok_or_else(|| AppError::protocol("当前不需要完成二次认证"))?;
        self.safety_verify_mfa_requested = false;
        let text = state.response.text();
        let sec_state = html::input_value(&text, "secState")
            .ok_or_else(|| AppError::protocol("二次认证页面缺少 secState 字段"))?;
        let execution = html::execution_value(&text)
            .ok_or_else(|| AppError::protocol("二次认证页面缺少 execution 字段"))?;
        let event_id = html::input_value(&text, "_eventId").unwrap_or_else(|| "submit".to_owned());
        let submit = html::input_value(&text, "submit").unwrap_or_else(|| "Login1".to_owned());

        // 二次認證表單同樣只提交給學校網域的主機。
        let submit_url = state.response.final_url.clone();
        ensure_trusted_submit_target(&submit_url)?;
        let response = self.post_form(
            &submit_url,
            vec![
                ("secState", sec_state),
                ("execution", execution),
                ("_eventId", event_id),
                ("geolocation", String::new()),
                ("fpVisitorId", self.visitor_id.clone()),
                ("submit", submit),
            ],
        )?;
        self.process_login_response(response)
    }

    fn finish_account_choice(&mut self) -> AppResult<LoginReply> {
        let response = self
            .choose_account_response
            .take()
            .ok_or_else(|| AppError::protocol("当前不需要选择账户"))?;
        let text = response.text();
        let choices = html::account_choices(&text)
            .ok_or_else(|| AppError::protocol("账户选择页面缺少选项"))?;
        let label = self
            .account_type
            .select(&choices)
            .ok_or_else(|| {
                AppError::protocol(format!("未找到{}身份的账户选项", self.account_type.label()))
            })?
            .to_owned();
        let execution = html::execution_value(&text).unwrap_or_default();

        let response = self.post_form(
            ACCOUNT_CHOICE_URL,
            vec![
                ("execution", execution),
                ("_eventId", "submit".to_owned()),
                ("geolocation", String::new()),
                ("fpVisitorId", self.visitor_id.clone()),
                ("trustAgent", self.trust_agent()),
                ("username", label),
                ("useDefault", "false".to_owned()),
            ],
        )?;
        response.error_for_status()?;

        self.has_login = true;
        self.final_response = Some(response);
        Ok(LoginReply::Success)
    }

    /// `trustAgent` 只有在已完成簡訊驗證時才帶值，其餘情況依參考實作為空字串。
    fn trust_agent(&self) -> String {
        if self.mfa.as_ref().is_some_and(|ctx| ctx.required) {
            "true".to_owned()
        } else {
            String::new()
        }
    }

    fn mfa_gid(&self) -> AppResult<String> {
        self.mfa
            .as_ref()
            .and_then(|ctx| ctx.gid.clone())
            .ok_or_else(|| AppError::protocol("缺少短信验证会话，请先获取验证码"))
    }

    /// 取得公鑰；只有解析成功才會寫入快取，避免把錯誤正文當成公鑰。
    fn public_key(&mut self) -> AppResult<RsaPublicKey> {
        if let Some(key) = self.rsa_public_key.clone() {
            return Ok(key);
        }
        let response = self
            .client
            .send(HttpRequest::get(rsa::PUBLIC_KEY_URL).header("Referer", self.post_url.clone()))?;
        response.error_for_status()?;

        let key = rsa::parse_public_key(&response.text())?;
        self.rsa_public_key = Some(key.clone());
        Ok(key)
    }

    fn post_form(&self, url: &str, fields: Vec<(&str, String)>) -> AppResult<HttpResponse> {
        self.client.send(HttpRequest::post_form(url, fields))
    }
}

/// 確認登入表單的提交目標位於學校網域且使用 https；否則回報錯誤
/// （訊息只含主機名或簡短原因，不含完整 URL）。
fn ensure_trusted_submit_target(url: &str) -> AppResult<()> {
    let parsed = Url::parse(url).map_err(|_| AppError::protocol("登录重定向地址缺少主机名"))?;
    let Some(host) = parsed.host_str() else {
        return Err(AppError::protocol("登录重定向地址缺少主机名"));
    };
    if !webvpn::is_school_host(host) {
        return Err(AppError::UntrustedHost {
            host: host.to_owned(),
        });
    }
    if parsed.scheme() != "https" {
        return Err(AppError::protocol("登录提交目标必须使用 https"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/login_test.rs"]
mod login_test;
