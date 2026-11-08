//! OtaS の状態の保存。
//!
//! - [`FileStateStore`]：ディレクトリに `<キー>.json` として保存する。一時ファイルに書いて
//!   `fsync` してから置き換えるので、途中で落ちても前の内容か新しい内容のどちらかが残る。
//! - [`MemoryStateStore`]：メモリ上に保存する（テスト用）。

use otas_ports::{StateError, StateStore};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// キーは英小文字・数字・`-`・`_` だけ（パスとして解釈されないように）。
fn check_key(key: &str) -> Result<(), StateError> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(StateError::InvalidKey(key.to_string()))
    }
}

/// ディレクトリに保存する。ディレクトリは所有者だけが使える権限（0700）、ファイルは 0600 にする。
#[derive(Debug)]
pub struct FileStateStore {
    dir: PathBuf,
    counter: AtomicU64,
}

impl FileStateStore {
    /// 保存先のディレクトリを開く（なければ作る）。
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StateError> {
        let dir = dir.into();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        Ok(Self {
            dir,
            counter: AtomicU64::new(0),
        })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

impl StateStore for FileStateStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, StateError> {
        check_key(key)?;
        match fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), StateError> {
        check_key(key)?;
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .dir
            .join(format!(".{key}.{}.{n}.tmp", std::process::id()));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(value)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, self.path(key))?;
        // 置き換え（rename）自体も確実に残るよう、ディレクトリを同期する
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

/// メモリ上に保存する（テスト用）。
#[derive(Debug, Default)]
pub struct MemoryStateStore {
    values: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryStateStore {
    /// 空の保存先を作る。
    pub fn new() -> Self {
        Self::default()
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, StateError> {
        check_key(key)?;
        let values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        Ok(values.get(key).cloned())
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), StateError> {
        check_key(key)?;
        let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        values.insert(key.to_string(), value.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otas_types::TaskId;
    use std::os::unix::fs::PermissionsExt;

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("otas-state-{}", TaskId::new()))
    }

    #[test]
    fn values_survive_reopening() {
        let d = dir();
        {
            let s = FileStateStore::open(&d).unwrap();
            assert_eq!(s.load("broker").unwrap(), None);
            s.save("broker", b"one").unwrap();
            s.save("broker", b"two").unwrap();
        }
        let s = FileStateStore::open(&d).unwrap();
        assert_eq!(s.load("broker").unwrap().unwrap(), b"two");
        // 一時ファイルは残らない
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn files_are_private() {
        let d = dir();
        let s = FileStateStore::open(&d).unwrap();
        s.save("supervisor", b"{}").unwrap();
        let mode = fs::metadata(d.join("supervisor.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let mode = fs::metadata(&d).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn keys_cannot_be_paths() {
        let s = MemoryStateStore::new();
        for bad in ["", "../etc/passwd", "a/b", "Broker", "a.json"] {
            assert!(
                matches!(s.save(bad, b"x"), Err(StateError::InvalidKey(_))),
                "{bad}"
            );
        }
        s.save("ok-key_1", b"x").unwrap();
    }
}
