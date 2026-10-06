//! 瀏覽器啟動命令測試。

use super::*;

#[test]
fn builds_platform_commands_with_separate_arguments() {
    let (program, args) = command_for("linux", "https://lms.xjtu.edu.cn").expect("命令");
    assert_eq!(program, "xdg-open");
    assert_eq!(args, vec!["https://lms.xjtu.edu.cn"]);

    let (program, args) = command_for("macos", "https://lms.xjtu.edu.cn/lesson?a=1").expect("命令");
    assert_eq!(program, "open");
    assert_eq!(args, vec!["https://lms.xjtu.edu.cn/lesson?a=1"]);

    // Windows：`rundll32 url.dll,FileProtocolHandler` 不經命令殼層
    //（`cmd /C start` 會把 URL 內的 `%` 當環境變數展開、`&` 當命令分隔）。
    let (program, args) = command_for("windows", "https://lms.xjtu.edu.cn").expect("命令");
    assert_eq!(program, "rundll32.exe");
    assert_eq!(
        args,
        vec!["url.dll,FileProtocolHandler", "https://lms.xjtu.edu.cn"]
    );

    // 參數不得把命令與網址併成單一字串，也不得經過命令殼層。
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("cmd") || arg.contains("start https")),
        "不得以 shell 字符串拼接：{args:?}"
    );
}

/// 網址原樣以單一參數傳遞：百分號編碼、`&`、空白與 Unicode 都不得被解讀。
#[test]
fn passes_urls_verbatim_on_every_platform() {
    let url = "https://lms.xjtu.edu.cn/search?q=%PATH%&x=1&u=张三 李四";
    for os in ["linux", "macos", "windows"] {
        let (_program, args) = command_for(os, url).expect("命令");
        assert!(
            args.iter().any(|arg| arg == url),
            "{os} 应原样传递网址：{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("%PATH%") && arg != url),
            "{os} 不得改写或逸出网址：{args:?}"
        );
    }
    let (_, args) = command_for("windows", url).expect("命令");
    assert_eq!(args[0], "url.dll,FileProtocolHandler", "参数必须分开传递");
    assert_eq!(args.len(), 2, "Windows 只应有处理器与网址两个参数");
}

#[test]
fn rejects_non_http_urls() {
    for url in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "not a url",
        "ftp://example.com/x",
        "",
    ] {
        assert!(
            command_for("linux", url).is_err(),
            "应拒绝非 http(s) 网址：{url}"
        );
    }
    assert!(open_url("javascript:alert(1)").is_err());
}

/// 只允許校內 `https` 網址：外部主機、後綴混淆與 http 降級都必須拒絕。
///
/// 要開啟的網址可能來自伺服器且附帶存取 token（思源學堂的播放地址），
/// 一旦被導向校外主機就等於把 token 交給第三方。
#[test]
fn rejects_urls_that_are_not_school_https() {
    for url in [
        "https://example.com/player?token=abc",
        "http://lms.xjtu.edu.cn/lesson",
        "https://lms.xjtu.edu.cn.evil.com/lesson?token=abc",
        "https://evil-xjtu.edu.cn/lesson",
        "https://xjtu.edu.cn.evil.com/",
    ] {
        // 實際入口是 `command_for`（`open_url` 也經由它）：確認命令不會被組出來。
        let err = command_for("linux", url).unwrap_err();
        assert!(
            matches!(err, AppError::UntrustedUrl { .. }),
            "应为网址拒绝错误：{url} → {err}"
        );
        let message = err.to_string();
        assert!(
            !message.contains("token"),
            "错误讯息不得含网址内容：{message}"
        );
        assert!(
            !message.contains("/lesson"),
            "错误讯息不得含路径：{message}"
        );
        // 單元層入口適用同一組規則。
        ensure_openable(url).unwrap_err();
    }

    // 校內 https（含 WebVPN 主機）仍可開啟，且命令組法不變。
    for url in [
        "https://lms.xjtu.edu.cn/lesson/player?token=abc",
        "https://webvpn.xjtu.edu.cn/https/77726476706e69737468656265737421/lesson",
        "https://xjtu.edu.cn/",
    ] {
        ensure_openable(url).unwrap_or_else(|err| panic!("校内 https 应允许：{url} → {err}"));
        let (program, args) = command_for("linux", url)
            .unwrap_or_else(|err| panic!("校内 https 应可组出命令：{err}"));
        assert_eq!(program, "xdg-open");
        assert_eq!(args, vec![url.to_owned()], "网址必须原样传递");
    }
}

/// 無法解析的網址同樣拒絕，且不使用「網址被拒」的訊息（避免誤導）。
#[test]
fn rejects_unparsable_urls_without_echoing_them() {
    let err = ensure_openable("https:// lms.xjtu.edu.cn/lesson?token=abc").unwrap_err();
    assert!(
        matches!(err, AppError::Protocol(_)),
        "格式错误应为协定错误：{err}"
    );
    assert!(
        !err.to_string().contains("token"),
        "错误讯息不得含网址内容：{err}"
    );
}

#[cfg(unix)]
#[test]
fn reap_launcher_reports_immediate_failure() {
    let failed = Command::new("false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 false");
    let err = reap_launcher(failed).expect_err("非零退出应报错");
    assert!(err.to_string().contains("退出码"), "应说明退出码：{err}");

    let ok = Command::new("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 true");
    reap_launcher(ok).expect("零退出应视为成功");
}
