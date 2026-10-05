//! 任務存儲的單元測試：加密往返、損毀備份、寫入失敗回滾與換口令重加密。

use std::fs;

use tempfile::{TempDir, tempdir};

use super::*;
use crate::domain::todo::Priority;

const PASSPHRASE: &str = "correct horse battery staple";

fn new_store(dir: &TempDir) -> TaskStore {
    TaskStore::at(dir.path().join("tasks.vault"))
}

fn task(content: &str) -> Task {
    Task {
        id: 0,
        content: content.to_owned(),
        description: None,
        deadline: None,
        priority: Priority::Low,
        completed: false,
    }
}

#[test]
fn first_save_creates_the_file_but_lazy_init_does_not() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    assert!(store.init(PASSPHRASE).unwrap().is_none());
    assert!(!store.path().exists(), "从没保存过任务时不应产生任务文件");

    store.add(task("写实验报告")).unwrap();
    assert!(store.path().is_file(), "首次保存后任务文件应当存在");
}

#[test]
fn round_trips_tasks_through_the_encrypted_file() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.add(task("甲")).unwrap();
    store.add(task("乙")).unwrap();
    let ids: Vec<u64> = store.tasks().iter().map(|task| task.id).collect();
    // 清單已依排序鍵排列（與加入順序無關），因此以內容找出要標記的任務。
    let done_id = store
        .tasks()
        .iter()
        .find(|task| task.content == "乙")
        .map(|task| task.id)
        .expect("应有乙");
    store.set_done(done_id, true).unwrap();

    let mut reloaded = new_store(&dir);
    reloaded.init(PASSPHRASE).unwrap();
    let tasks = reloaded.tasks();
    assert_eq!(tasks.len(), 2);
    let contents: Vec<&str> = tasks.iter().map(|task| task.content.as_str()).collect();
    assert!(contents.contains(&"甲") && contents.contains(&"乙"));
    assert!(
        tasks
            .iter()
            .any(|task| task.content == "乙" && task.completed),
        "完成状态应当被保存"
    );

    // 識別碼不重用：刪掉中間一項後新增的識別碼仍遞增。
    reloaded.delete(ids[0]).unwrap();
    reloaded.add(task("丙")).unwrap();
    let newest = reloaded.tasks().iter().map(|task| task.id).max().unwrap();
    assert!(newest > ids[1], "新增任务的识别码不应重复使用：{newest}");
}

#[test]
fn the_file_itself_contains_no_plaintext() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.add(task("TOP-SECRET-CONTENT")).unwrap();

    let bytes = fs::read(store.path()).unwrap();
    assert!(
        !bytes
            .windows("TOP-SECRET-CONTENT".len())
            .any(|window| window == b"TOP-SECRET-CONTENT"),
        "任务文件不得包含明文内容"
    );
    // 以其他口令載入：無法解開，原檔保持原樣並標記不可用。
    let mut other = new_store(&dir);
    let notice = other.init("another passphrase").unwrap().expect("提示");
    assert!(notice.contains("未被修改"));
    assert!(other.tasks().is_empty());
}

#[test]
fn unreadable_file_is_left_untouched_and_marks_the_store_unavailable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    fs::write(&path, b"not an envelope at all").unwrap();

    let mut store = new_store(&dir);
    let notice = store.init(PASSPHRASE).unwrap().expect("应当有提示");
    assert!(
        notice.contains("未被修改"),
        "提示应说明原文件未被修改：{notice}"
    );
    assert!(
        notice.contains("tasks.vault"),
        "提示应包含原文件路径：{notice}"
    );
    assert!(store.tasks().is_empty(), "无法读取时不得凭空产生任务");
    assert_eq!(
        fs::read(&path).unwrap(),
        b"not an envelope at all",
        "原文件必须保持原样"
    );
    assert!(
        !dir.path().join("tasks.vault.bak").exists(),
        "不得再产生 .bak 备份"
    );

    // 存儲被標記為不可用：後續操作回報原因，且不覆寫原文件。
    let err = store.add(task("新的任务")).unwrap_err();
    assert!(
        matches!(err, AppError::Crypto(_)),
        "不可用时保存应回报明确错误：{err:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        b"not an envelope at all",
        "不可用时也不得改动原文件"
    );
}

#[test]
fn a_file_from_another_passphrase_is_left_untouched() {
    let dir = tempdir().unwrap();
    let mut first = new_store(&dir);
    first.init(PASSPHRASE).unwrap();
    first.add(task("旧口令下的任务")).unwrap();
    let before = fs::read(dir.path().join("tasks.vault")).unwrap();

    let mut second = new_store(&dir);
    let notice = second
        .init("a different passphrase")
        .unwrap()
        .expect("提示");
    assert!(notice.contains("未被修改"));
    assert!(second.tasks().is_empty());
    assert_eq!(
        fs::read(dir.path().join("tasks.vault")).unwrap(),
        before,
        "原文件必须保持原样"
    );
    assert!(!dir.path().join("tasks.vault.bak").exists());
}

#[test]
fn update_and_batch_operations_report_their_effects() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.add(task("甲")).unwrap();
    store.add(task("乙")).unwrap();
    store.add(task("丙")).unwrap();
    let ids: Vec<u64> = store.tasks().iter().map(|task| task.id).collect();

    let mut edited = task("甲（改）");
    edited.priority = Priority::High;
    store.update(ids[0], edited).unwrap();
    assert_eq!(store.tasks()[0].content, "甲（改）");
    assert_eq!(store.tasks()[0].priority, Priority::High);

    // 已完成的才計入變更數。
    store.set_done(ids[1], true).unwrap();
    let changed = store.set_done_many(&ids, true).unwrap();
    assert_eq!(changed, 2, "甲与丙的状态有变化，乙原本就已完成");

    let deleted = store.delete_completed().unwrap();
    assert_eq!(deleted, 3);
    assert!(store.tasks().is_empty());

    store.add(task("丁")).unwrap();
    let remaining: Vec<u64> = store.tasks().iter().map(|task| task.id).collect();
    assert_eq!(store.delete_many(&remaining).unwrap(), 1);
    assert_eq!(store.delete_many(&remaining).unwrap(), 0);

    // 找不到任務時回報明確錯誤，且不改變內容。
    assert!(matches!(
        store.delete(999).unwrap_err(),
        AppError::TaskNotFound
    ));
    assert!(matches!(
        store.set_done(999, true).unwrap_err(),
        AppError::TaskNotFound
    ));
}

#[cfg(unix)]
#[test]
fn failed_save_rolls_back_the_in_memory_change() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempdir().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    let mut store = TaskStore::at(data.join("tasks.vault"));
    store.init(PASSPHRASE).unwrap();
    store.add(task("先保存一次")).unwrap();

    fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).unwrap();
    // 若當前使用者不受權限限制（例如 root），此測試沒有意義。
    if fs::write(data.join("probe"), b"x").is_ok() {
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let before = store.snapshot();
    let result = store.add(task("写不进去"));
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(result.is_err(), "目录不可写时保存应当失败");
    assert_eq!(store.snapshot(), before, "保存失败时内存状态必须回滚");
}

#[test]
fn rekey_moves_the_file_to_the_new_passphrase() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.add(task("换口令前的任务")).unwrap();

    store.rekey("a brand new passphrase").unwrap();

    let mut reloaded = new_store(&dir);
    reloaded.init("a brand new passphrase").unwrap();
    assert_eq!(reloaded.tasks().len(), 1, "新口令应能解开任务文件");

    // 換口令後仍可繼續保存。
    reloaded.add(task("换口令后的任务")).unwrap();
    let mut again = new_store(&dir);
    again.init("a brand new passphrase").unwrap();
    assert_eq!(again.tasks().len(), 2);
}

#[test]
fn rekey_is_refused_when_the_store_is_unavailable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    fs::write(&path, b"not an envelope at all").unwrap();

    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap().expect("应当有提示");

    // 存儲不可用時換口令必須被拒絕：不得以記憶體中的空任務覆寫原檔。
    let err = store.rekey("a brand new passphrase").unwrap_err();
    assert!(
        matches!(err, AppError::Crypto(_)),
        "不可用时换口令应被拒绝：{err:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        b"not an envelope at all",
        "被拒绝的换口令不得改动原文件"
    );
    assert!(
        !dir.path().join("tasks.vault.bak").exists(),
        "不得产生任何备份"
    );
}

#[test]
fn a_failed_reinit_drops_the_loaded_tasks_and_blocks_writes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tasks.vault");
    let mut store = new_store(&dir);
    // 先成功載入一份任務（`sealed` 與 `tasks` 都有內容）。
    store.init(PASSPHRASE).unwrap();
    store.add(task("第一次加载")).unwrap();
    assert_eq!(store.tasks().len(), 1);

    // 同一個實例再次初始化：原檔已被換成無法解讀的內容。
    fs::write(&path, b"corrupted by someone else").unwrap();
    let notice = store.init(PASSPHRASE).unwrap().expect("应当有提示");
    assert!(notice.contains("未被修改"));
    assert!(
        store.tasks().is_empty(),
        "再次初始化失败后不得保留先前的任务"
    );

    // 後續任何寫入都必須失敗，且不得改動原檔。
    let err = store.add(task("第二次")).unwrap_err();
    assert!(
        matches!(err, AppError::Crypto(_)),
        "不可用时的保存应被拒绝：{err:?}"
    );
    assert!(store.rekey("another passphrase").is_err());
    assert_eq!(
        fs::read(&path).unwrap(),
        b"corrupted by someone else",
        "原文件必须保持原样"
    );
}

#[test]
fn mark_unavailable_blocks_saves_even_with_a_loaded_key() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.add(task("甲")).unwrap();

    // 直接標記不可用（`load()` 的硬錯誤路徑）時，記憶體中的金鑰仍在。
    store.mark_unavailable("任务文件不可用".to_owned());
    let err = store.add(task("乙")).unwrap_err();
    assert!(
        matches!(err, AppError::Crypto(_)),
        "不可用时的保存应被拒绝：{err:?}"
    );

    // 原檔仍只有第一筆。
    let mut reloaded = new_store(&dir);
    reloaded.init(PASSPHRASE).unwrap();
    assert_eq!(reloaded.tasks().len(), 1, "不得写入第二条任务");
}
