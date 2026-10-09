//! 同步引擎的單元測試：計畫判斷、導入、上傳／下載與衝突分流。

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;

use super::{Plan, evaluate, fingerprint, import, plan, pull, push};
use crate::error::AppError;
use crate::http::fake::FakeClient;
use crate::http::{HttpResponse, Method};
use crate::sync::config::{FileRecord, SyncFile};
use crate::sync::webdav::WebDav;

/// 建立帶標頭的回應。
fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        final_url: "https://dav.example/dav/file".to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
    }
}

fn dav(responses: Vec<HttpResponse>) -> (Arc<FakeClient>, WebDav) {
    let client = Arc::new(FakeClient::new(responses));
    let dav = WebDav::new(client.clone(), "https://dav.example/dav", "u", "p");
    (client, dav)
}

/// 建立結構完整的加密信封：同步只驗容器結構，不驗口令。
///
/// 遠端存放的就是本機容器的原始位元組，因此測試也必須用真的信封——把
/// 任意位元組當成雲端內容會在下載把關時被擋下（那正是 `ensure_container`）。
fn container(plaintext: &[u8]) -> Vec<u8> {
    crate::credentials::envelope::seal("pw", "ohmyXJTU-vault", plaintext)
        .expect("建立测试信封")
        .0
}

#[test]
fn plan_covers_all_four_combinations() {
    assert_eq!(plan(false, false), Plan::Noop);
    assert_eq!(plan(true, false), Plan::Push);
    assert_eq!(plan(false, true), Plan::Pull);
    assert_eq!(plan(true, true), Plan::Conflict);
}

#[test]
fn fingerprint_changes_with_content() {
    assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
    assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
}

#[test]
fn import_writes_files_and_returns_records() {
    let dir = tempdir().unwrap();
    let credentials = dir.path().join("credentials.vault");
    let tasks = dir.path().join("tasks.vault");
    let cred_bytes = container(b"cred-bytes");
    let task_bytes = container(b"task-bytes");
    let (_client, dav) = dav(vec![
        response(200, &[("ETag", "c1")], &cred_bytes),
        response(200, &[("ETag", "t1")], &task_bytes),
    ]);
    let targets: Vec<(SyncFile, PathBuf)> = vec![
        (SyncFile::Credentials, credentials.clone()),
        (SyncFile::Tasks, tasks.clone()),
    ];
    let records = import(&dav, &targets).expect("导入应成功");
    assert_eq!(std::fs::read(&credentials).unwrap(), cred_bytes);
    assert_eq!(std::fs::read(&tasks).unwrap(), task_bytes);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, SyncFile::Credentials);
    assert_eq!(records[0].1.version.as_deref(), Some("c1"));
}

#[test]
fn import_errors_when_the_remote_is_empty() {
    let dir = tempdir().unwrap();
    let (_client, dav) = dav(vec![response(404, &[], b""), response(404, &[], b"")]);
    let targets: Vec<(SyncFile, PathBuf)> = vec![
        (SyncFile::Credentials, dir.path().join("c.vault")),
        (SyncFile::Tasks, dir.path().join("t.vault")),
    ];
    assert!(import(&dav, &targets).is_err());
}

#[test]
fn pull_writes_the_remote_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("credentials.vault");
    let bytes = container(b"remote");
    let (_client, dav) = dav(vec![response(200, &[("ETag", "v9")], &bytes)]);
    let record = pull(&dav, SyncFile::Credentials, &path)
        .expect("下载应成功")
        .expect("应有内容");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(record.version.as_deref(), Some("v9"));
    assert_eq!(record.hash.as_deref(), Some(fingerprint(&bytes).as_str()));
}

/// 雲端回的不是本程式的容器（指到錯的目錄、供應商錯誤頁…）：不得覆寫本機檔案。
#[test]
fn pull_refuses_to_overwrite_the_local_file_with_foreign_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("credentials.vault");
    std::fs::write(&path, b"local-vault").unwrap();
    let (_client, dav) = dav(vec![response(
        200,
        &[("ETag", "v1")],
        b"<html>not a vault</html>",
    )]);
    assert!(matches!(
        pull(&dav, SyncFile::Credentials, &path),
        Err(AppError::WebDav(_))
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"local-vault",
        "本机文件不得被覆盖"
    );
}

/// 導入時同樣把關：遇到不合格的檔案就整批失敗，本機檔案保持原狀。
#[test]
fn import_refuses_foreign_content() {
    let dir = tempdir().unwrap();
    let credentials = dir.path().join("credentials.vault");
    let tasks = dir.path().join("tasks.vault");
    std::fs::write(&credentials, b"local-cred").unwrap();
    let good = container(b"remote-tasks");
    let (_client, dav) = dav(vec![
        response(200, &[("ETag", "c1")], b"<html>nope</html>"),
        response(200, &[("ETag", "t1")], &good),
    ]);
    let targets: Vec<(SyncFile, PathBuf)> = vec![
        (SyncFile::Credentials, credentials.clone()),
        (SyncFile::Tasks, tasks.clone()),
    ];
    assert!(import(&dav, &targets).is_err(), "非容器应使导入失败");
    assert_eq!(std::fs::read(&credentials).unwrap(), b"local-cred");
    assert!(!tasks.exists(), "后续文件不应被写入");
}

#[test]
fn push_sends_local_content_and_records_the_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"local-bytes").unwrap();
    let (client, dav) = dav(vec![response(201, &[("ETag", "n1")], b"")]);
    let record = push(&dav, SyncFile::Tasks, &path, None).expect("上传应成功");
    assert_eq!(record.version.as_deref(), Some("n1"));

    let request = client.last_request().expect("应有请求");
    assert_eq!(request.method, Method::Put);
}

#[test]
fn push_sends_if_match_and_records_the_new_etag() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"local-bytes").unwrap();
    let (client, dav) = dav(vec![response(201, &[("ETag", "\"n2\"")], b"")]);
    let record = push(&dav, SyncFile::Tasks, &path, Some("\"n1\"")).expect("上传应成功");
    assert_eq!(record.version.as_deref(), Some("n2"));
    assert_eq!(record.etag.as_deref(), Some("\"n2\""));

    let request = client.last_request().expect("应有请求");
    assert_eq!(request.header_value("If-Match"), Some("\"n1\""));
}

#[test]
fn push_surfaces_a_conflict_on_precondition_failure() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"local-bytes").unwrap();
    let (_client, dav) = dav(vec![response(412, &[], b"")]);
    assert!(matches!(
        push(&dav, SyncFile::Tasks, &path, Some("\"stale\"")),
        Err(AppError::WebDavConflict)
    ));
}

#[test]
fn pull_returns_none_when_the_remote_has_no_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    let (_client, dav) = dav(vec![response(404, &[], b"")]);
    assert!(
        pull(&dav, SyncFile::Tasks, &path)
            .expect("下载不应报错")
            .is_none()
    );
}

#[test]
fn push_errors_when_there_is_no_local_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    let (_client, dav) = dav(vec![]);
    assert!(matches!(
        push(&dav, SyncFile::Tasks, &path, None),
        Err(AppError::WebDav(_))
    ));
}

#[test]
fn evaluate_reports_only_local_change_as_push() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"newer").unwrap();
    let record = FileRecord {
        version: Some("v1".to_owned()),
        etag: None,
        hash: Some(fingerprint(b"older")),
    };
    let (_client, dav) = dav(vec![response(200, &[("ETag", "v1")], b"")]);
    assert_eq!(
        evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap(),
        Plan::Push
    );
}

#[test]
fn evaluate_reports_conflict_when_both_changed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"newer").unwrap();
    let record = FileRecord {
        version: Some("v1".to_owned()),
        etag: None,
        hash: Some(fingerprint(b"older")),
    };
    let (_client, dav) = dav(vec![response(200, &[("ETag", "v2")], b"")]);
    assert_eq!(
        evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap(),
        Plan::Conflict
    );
}
