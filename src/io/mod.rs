//! 檔案系統存取：資料目錄解析與私有（0600）檔案讀寫。

pub mod paths;
pub mod secure_file;

pub use paths::{captcha_path, config_path, data_dir, vault_path};
pub use secure_file::{read_private, restrict_permissions, write_private_atomic};
