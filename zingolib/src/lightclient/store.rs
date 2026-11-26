//! Where a [`super::LightClient`] keeps its wallet bytes.

use std::{
    fs::{File, OpenOptions},
    io::{self, ErrorKind, Write as _},
    path::{Path, PathBuf},
};

/// Persistence for the serialized wallet. A platform without a plain file system supplies its own.
pub trait WalletStore: Send + Sync + 'static {
    /// The stored wallet bytes, or `None` if no wallet was written yet.
    fn load(&self) -> io::Result<Option<Vec<u8>>>;

    /// Replaces the stored wallet bytes.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;

    /// Removes the stored wallet.
    fn delete(&self) -> io::Result<()>;
}

/// A wallet file on the local file system, replaced atomically on every write.
#[derive(Clone, Debug)]
pub struct FileStore(PathBuf);

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl WalletStore for FileStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.0) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let parent = self.0.parent().filter(|dir| !dir.as_os_str().is_empty());
        if let Some(dir) = parent {
            std::fs::create_dir_all(dir)?;
        }
        let temp_path = self.0.with_extension(
            self.0
                .extension()
                .map(|e| format!("{}.tmp", e.to_string_lossy()))
                .unwrap_or_else(|| "tmp".to_string()),
        );
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &self.0)?;

        #[cfg(unix)]
        if let Some(dir) = parent {
            let _ignore_error = File::open(dir)?.sync_all();
        }

        Ok(())
    }

    fn delete(&self) -> io::Result<()> {
        std::fs::remove_file(&self.0)
    }
}
