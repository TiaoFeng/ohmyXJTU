//! 同步引擎的單元測試：計畫判斷、導入、上傳／下載與衝突分流。

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;

use super::{Plan, evaluate, fingerprint, import, local_changed, plan, pull, push};
use crate::error::AppError;
use crate::http::fake::FakeClient;
use crate::http::{HttpResponse, Method};
use crate::sync::config::{FileRecord, SyncFile};
use crate::sync::webdav::{Precondition, WebDav};

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

/// `local_changed`：比對指紋、記錄缺指紋或有檔視為已變更、檔案不存在則否。
#[test]
fn local_changed_compares_against_the_recorded_hash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");

    // 檔案不存在：不算變更（與「檔案缺失」一致）。
    let record = FileRecord {
        hash: Some(fingerprint(b"whatever")),
        ..FileRecord::default()
    };
    assert!(!local_changed(&path, &record).unwrap());

    std::fs::write(&path, b"content").unwrap();
    assert!(
        !local_changed(
            &path,
            &FileRecord {
                hash: Some(fingerprint(b"content")),
                ..FileRecord::default()
            }
        )
        .unwrap(),
        "指紋相同應視為未變更"
    );
    assert!(
        local_changed(
            &path,
            &FileRecord {
                hash: Some(fingerprint(b"older")),
                ..FileRecord::default()
            }
        )
        .unwrap(),
        "指紋不同應視為已變更"
    );
    assert!(
        local_changed(&path, &FileRecord::default()).unwrap(),
        "記錄缺指紋但有本機檔應視為已變更"
    );
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
    let record = push(&dav, SyncFile::Tasks, &path, Precondition::Any).expect("上传应成功");
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
    let record =
        push(&dav, SyncFile::Tasks, &path, Precondition::Match("\"n1\"")).expect("上传应成功");
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
        push(
            &dav,
            SyncFile::Tasks,
            &path,
            Precondition::Match("\"stale\"")
        ),
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
        push(&dav, SyncFile::Tasks, &path, Precondition::Any),
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
        evaluate(&dav, SyncFile::Tasks, &path, &record)
            .unwrap()
            .plan,
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
        evaluate(&dav, SyncFile::Tasks, &path, &record)
            .unwrap()
            .plan,
        Plan::Conflict
    );
}

/// 遠端確認不存在時的 Push 要帶「必須不存在」條件；遠端存在則沿用上次 ETag。
#[test]
fn evaluate_prefers_must_not_exist_only_when_the_remote_is_absent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"newer").unwrap();

    // 遠端 404：本機變更 → Push，且前條件為 MustNotExist。
    let (_client_a, dav_a) = dav(vec![response(404, &[], b"")]);
    let record = FileRecord::default();
    let evaluation = evaluate(&dav_a, SyncFile::Tasks, &path, &record).unwrap();
    assert_eq!(evaluation.plan, Plan::Push);
    assert!(evaluation.remote_absent);
    assert_eq!(
        evaluation.precondition(None),
        Precondition::MustNotExist,
        "远端不存在时应要求必须不存在"
    );

    // 遠端存在且有 ETag：前條件沿用上次記錄的 ETag（Match）。
    let record = FileRecord {
        version: Some("v1".to_owned()),
        etag: Some("\"v1\"".to_owned()),
        hash: Some(fingerprint(b"newer")),
    };
    let (_client_b, dav_b) = dav(vec![response(200, &[("ETag", "v1")], b"")]);
    let evaluation = evaluate(&dav_b, SyncFile::Tasks, &path, &record).unwrap();
    assert!(!evaluation.remote_absent);
    assert_eq!(
        evaluation.precondition(record.etag.as_deref()),
        Precondition::Match("\"v1\"")
    );
}

/// 雲端檔案消失（被刪除，或換到新的空伺服器）時，本機有檔就要補上。
///
/// 舊碼把「遠端 404」與「遠端沒變」混為一談：本機指紋與記錄相符時整個同步
/// 成了 `Noop`，「立即同步」回報成功卻什麼都沒上傳，雲端永遠補不回來。
#[test]
fn evaluate_pushes_when_the_remote_file_disappeared() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"content").unwrap();
    // 上次同步成功（版本與本機指紋都有記錄），此後本機未再變更。
    let record = FileRecord {
        version: Some("v1".to_owned()),
        etag: Some("\"v1\"".to_owned()),
        hash: Some(fingerprint(b"content")),
    };
    let (_client, dav) = dav(vec![response(404, &[], b"")]);
    let evaluation = evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap();
    assert_eq!(evaluation.plan, Plan::Push, "远端消失时应由本机补上");
    assert!(evaluation.remote_absent);
    assert_eq!(
        evaluation.precondition(record.etag.as_deref()),
        Precondition::MustNotExist,
        "补上远端缺失的档案时应要求必须不存在"
    );
}

/// 兩邊都沒有內容（本機沒有檔、遠端也沒有）時不得動作。
#[test]
fn evaluate_stays_noop_when_neither_side_has_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    let record = FileRecord::default();
    let (_client, dav) = dav(vec![response(404, &[], b"")]);
    let evaluation = evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap();
    assert_eq!(evaluation.plan, Plan::Noop, "两边都没有东西时不该动作");
    assert!(evaluation.remote_absent);
}

/// 伺服器完全不提供版本資訊時無法偵測遠端變更（已知限制，鎖住行為）。
#[test]
fn evaluate_cannot_detect_remote_changes_without_any_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    std::fs::write(&path, b"content").unwrap();
    let record = FileRecord {
        version: None,
        etag: None,
        hash: Some(fingerprint(b"content")),
    };
    let (_client, dav) = dav(vec![response(200, &[], b"")]);
    let evaluation = evaluate(&dav, SyncFile::Tasks, &path, &record).unwrap();
    assert_eq!(
        evaluation.plan,
        Plan::Noop,
        "没有版本可比时只能假设远端没变"
    );
    assert!(!evaluation.remote_absent);
}
