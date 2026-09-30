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
        "不得以 shell 字串拼接：{args:?}"
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
            "{os} 應原樣傳遞網址：{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("%PATH%") && arg != url),
            "{os} 不得改寫或逸出網址：{args:?}"
        );
    }
    let (_, args) = command_for("windows", url).expect("命令");
    assert_eq!(args[0], "url.dll,FileProtocolHandler", "參數必須分開傳遞");
    assert_eq!(args.len(), 2, "Windows 只應有處理器與網址兩個參數");
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
            "應拒絕非 http(s) 網址：{url}"
        );
    }
    assert!(open_url("javascript:alert(1)").is_err());
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
