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

    // Windows：`start` 是 cmd 內建指令，網址必須是獨立參數。
    let (program, args) = command_for("windows", "https://lms.xjtu.edu.cn").expect("命令");
    assert_eq!(program, "cmd");
    assert_eq!(args, vec!["/C", "start", "", "https://lms.xjtu.edu.cn"]);

    // 參數不得把命令與網址併成單一字串。
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("cmd") || arg.contains("start https")),
        "不得以 shell 字串拼接：{args:?}"
    );
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
