//! Run files in memory for the use cases' unit tests.

use anyhow::Result;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::RunFiles;

/// Run files in memory, each with the time it was written.
#[derive(Default)]
pub struct MemoryFiles {
    files: Mutex<HashMap<PathBuf, (SystemTime, Vec<u8>)>>,
}

impl MemoryFiles {
    pub fn put(&self, path: &Path, modified: SystemTime, contents: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (modified, contents.as_bytes().to_vec()));
    }
}

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl RunFiles for MemoryFiles {
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn create_new_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.put(path, self.now(), &String::from_utf8_lossy(contents));
        Ok(())
    }
    fn copy(&self, _: &Path, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let files = self.files.lock().unwrap();
        files
            .get(path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(missing)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        let files = self.files.lock().unwrap();
        files.get(path).map(|(at, _)| *at).ok_or_else(missing)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }
    fn is_file(&self, path: &Path) -> bool {
        self.exists(path)
    }
    fn is_dir(&self, _: &Path) -> bool {
        false
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        Ok(files
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .cloned()
            .collect())
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        unimplemented!("no test renames a run file")
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        unimplemented!("no test removes a run file")
    }
    fn append_line(&self, _: &Path, _: &str) -> io::Result<()> {
        unimplemented!("no test appends to a run file")
    }
    fn canonicalize(&self, _: &Path) -> io::Result<PathBuf> {
        unimplemented!("no test resolves a run file")
    }
    fn write_fenced(&self, _: &Path, _: &str, _: &str, _: &Path) -> Result<()> {
        unimplemented!("no test writes a review")
    }
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000)
    }
}
