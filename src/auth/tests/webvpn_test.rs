//! WebVPN 網址轉換測試。
//!
//! 期望值由參考實作（`ref/auth/util.py`）實際計算後脫敏保存，確保兩個實作位元級一致。

use super::*;

/// 由參考實作產生的主機名密文。
const CIPHER_BK_KQ: &str = "f2fc0c97367e705a6a1dc7a99c406d36d5";
const CIPHER_LMS: &str = "fcfa52d23f3a7c45300d8db9d6562d";
const CIPHER_LOGIN: &str = "fcf84695297e705a6a1dc7a99c406d3655";
const CIPHER_CNKI: &str = "fbf952d2243e635930068cb8";

#[test]
fn encrypts_host_like_reference_implementation() {
    assert_eq!(encrypt_host("bk-kq.xjtu.edu.cn"), CIPHER_BK_KQ);
    assert_eq!(encrypt_host("lms.xjtu.edu.cn"), CIPHER_LMS);
    assert_eq!(encrypt_host("login.xjtu.edu.cn"), CIPHER_LOGIN);
    assert_eq!(encrypt_host("kns.cnki.net"), CIPHER_CNKI);
}

#[test]
fn decrypts_host_like_reference_implementation() {
    assert_eq!(decrypt_host(CIPHER_BK_KQ).unwrap(), "bk-kq.xjtu.edu.cn");
    assert_eq!(decrypt_host(CIPHER_LMS).unwrap(), "lms.xjtu.edu.cn");
    assert_eq!(decrypt_host(CIPHER_CNKI).unwrap(), "kns.cnki.net");
}

#[test]
fn converts_urls_to_webvpn() {
    assert_eq!(
        to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc").unwrap(),
        format!(
            "https://webvpn.xjtu.edu.cn/https/{PREFIX}{CIPHER_BK_KQ}/sa/auth/cas/login/student-pc"
        )
    );
    assert_eq!(
        to_webvpn_url("https://lms.xjtu.edu.cn/api/my-courses?page=1").unwrap(),
        format!("https://webvpn.xjtu.edu.cn/https/{PREFIX}{CIPHER_LMS}/api/my-courses?page=1")
    );
}

#[test]
fn converts_urls_with_port() {
    assert_eq!(
        to_webvpn_url("http://rg.lib.xjtu.edu.cn:8086/seat/").unwrap(),
        "https://webvpn.xjtu.edu.cn/http-8086/77726476706e69737468656265737421e2f00f902e322648741c9ce29d51367bae38/seat/"
    );
}

#[test]
fn converts_back_to_plain_url() {
    let vpn = format!(
        "https://webvpn.xjtu.edu.cn/https/{PREFIX}{CIPHER_CNKI}/KCMS/detail/detail.aspx?dbcode=CJFQ"
    );
    assert_eq!(
        from_webvpn_url(&vpn).unwrap(),
        "https://kns.cnki.net/KCMS/detail/detail.aspx?dbcode=CJFQ"
    );
}

#[test]
fn round_trips_urls() {
    for url in [
        "https://bk-kq.xjtu.edu.cn/sa/student/home",
        "https://lms.xjtu.edu.cn/api/my-courses",
        "http://rg.lib.xjtu.edu.cn:8086/seat/?room=1",
    ] {
        let vpn = to_webvpn_url(url).unwrap();
        assert_eq!(
            from_webvpn_url(&vpn).unwrap(),
            url,
            "往返转换应当一致：{url}"
        );
    }
}

#[test]
fn rejects_malformed_webvpn_urls() {
    assert!(from_webvpn_url("https://lms.xjtu.edu.cn/api").is_err());
    assert!(from_webvpn_url("https://webvpn.xjtu.edu.cn/https/short").is_err());
    assert!(from_webvpn_url(&format!("https://webvpn.xjtu.edu.cn/https/0000{PREFIX}ab/")).is_err());
    assert!(decrypt_host("zz").is_err());
    assert!(decrypt_host("abc").is_err());
}

#[test]
fn decides_which_urls_need_rewriting() {
    assert!(should_rewrite("https://bk-kq.xjtu.edu.cn/sa/student/home"));
    assert!(should_rewrite("https://xjtu.edu.cn/"));
    assert!(!should_rewrite("https://webvpn.xjtu.edu.cn/https/abc/"));
    assert!(!should_rewrite("https://lms.xjtu.edu.cn.evil.com/"));
    assert!(!should_rewrite("https://example.com/"));
    assert!(!should_rewrite("not a url"));
    assert!(!should_rewrite("mailto:someone@xjtu.edu.cn"));
}

#[test]
fn recognizes_school_hosts_without_suffix_confusion() {
    assert!(is_school_host("xjtu.edu.cn"));
    assert!(is_school_host("login.xjtu.edu.cn"));
    assert!(is_school_host("Login.XJTU.edu.cn"), "大小写不敏感");
    assert!(!is_school_host("lms.xjtu.edu.cn.evil.com"));
    assert!(!is_school_host("evilxjtu.edu.cn"));
    assert!(!is_school_host("xjtu.edu.cn.attacker.net"));
    assert!(!is_school_host("example.com"));
}
