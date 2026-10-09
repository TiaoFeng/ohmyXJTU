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
    let (_client, dav) = dav(vec![
        response(200, &[("ETag", "c1")], b"cred-bytes"),
        response(200, &[("ETag", "t1")], b"task-bytes"),
    ]);
    let targets: Vec<(SyncFile, PathBuf)> = vec![
        (SyncFile::Credentials, credentials.clone()),
        (SyncFile::Tasks, tasks.clone()),
    ];
    let records = import(&dav, &targets).expect("导入应成功");
    assert_eq!(std::fs::read(&credentials).unwrap(), b"cred-bytes");
    assert_eq!(std::fs::read(&tasks).unwrap(), b"task-bytes");
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
    let (_client, dav) = dav(vec![response(200, &[("ETag", "v9")], b"remote")]);
    let record = pull(&dav, SyncFile::Credentials, &path)
        .expect("下载应成功")
        .expect("应有内容");
    assert_eq!(std::fs::read(&path).unwrap(), b"remote");
    assert_eq!(record.version.as_deref(), Some("v9"));
    assert_eq!(
        record.hash.as_deref(),
        Some(fingerprint(b"remote").as_str())
    );
}

#[test]
fn push_sends_local_content_and_records_the_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"local-bytes").unwrap();
    let (client, dav) = dav(vec![response(201, &[("ETag", "n1")], b"")]);
    let record = push(&dav, SyncFile::Tasks, &path).expect("上传应成功");
    assert_eq!(record.version.as_deref(), Some("n1"));

    let request = client.last_request().expect("应有请求");
    assert_eq!(request.method, Method::Put);
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
        push(&dav, SyncFile::Tasks, &path),
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
        hash: Some(fingerprint(b"older")),
    };
    let (_client, dav) = dav(vec![response(200, &[("ETag", "v2")], b"")]);
    assert_eq!(
        evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap(),
        Plan::Conflict
    );
}
