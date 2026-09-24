//! The run directory on the local file system ([`RunFiles`]).

use std::{
    fs,
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};

use crate::application::RunFiles;

/// The run files as the local file system holds them.
pub struct LocalRunFiles;

impl RunFiles for LocalRunFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        fs::write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::copy(from, to).map(|_| ())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        fs::metadata(path)?.modified()
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let modified = file.metadata()?.modified()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).context("read idle marker")?;
        Ok(Some((modified, bytes)))
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(fs::read_dir(dir)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        let (longest, last) = backtick_run_and_last_byte(body)?;
        let fence = "`".repeat(longest.max(2) + 1);
        let mut out = BufWriter::new(
            fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
        );
        writeln!(out, "{text}{fence}{info}")?;
        io::copy(
            &mut fs::File::open(body).with_context(|| format!("open {}", body.display()))?,
            &mut out,
        )?;
        if last.is_some_and(|byte| byte != b'\n') {
            out.write_all(b"\n")?;
        }
        writeln!(out, "{fence}")?;
        out.into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()
            .with_context(|| format!("write {}", path.display()))
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The longest run of backticks in the file and its last byte, read in chunks.
fn backtick_run_and_last_byte(path: &Path) -> Result<(usize, Option<u8>)> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let (mut longest, mut run, mut last) = (0, 0, None);
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok((longest, last));
        }
        for &byte in &buffer[..read] {
            run = if byte == b'`' { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        last = Some(buffer[read - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn local_run_files_read_what_they_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalRunFiles;
        let run_dir = dir.path().join("runs/a");
        files.create_dir_all(&dir.path().join("runs")).unwrap();
        files.create_new_dir(&run_dir).unwrap();
        assert!(files.create_new_dir(&run_dir).is_err());
        assert!(files.is_dir(&run_dir) && !files.is_file(&run_dir));
        let prompt = run_dir.join("prompt.txt");
        files.write(&prompt, b"hello").unwrap();
        files.copy(&prompt, &run_dir.join("copy.txt")).unwrap();
        assert_eq!(
            files.read_to_string(&run_dir.join("copy.txt")).unwrap(),
            "hello"
        );
        assert_eq!(files.read(&prompt).unwrap(), b"hello");
        assert!(files.exists(&prompt) && files.is_file(&prompt));
        let modified = files.modified(&prompt).unwrap();
        assert!(modified <= files.now() + Duration::from_secs(1));
        let (stamped, bytes) = files.read_stamped(&prompt).unwrap().unwrap();
        assert_eq!((stamped, bytes.as_slice()), (modified, b"hello".as_slice()));
        assert!(files.read_stamped(&run_dir.join("none")).unwrap().is_none());
        assert!(files.read_stamped(&run_dir).is_err());
        assert!(files.modified(&run_dir.join("none")).is_err());
    }
}
