use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPage {
    pub url: String,
    pub title: Option<String>,
    pub content: String,
}

pub struct PageStore {
    file: Mutex<BufWriter<File>>,
}

impl PageStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;

        Ok(Self {
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn append(&self, page: &StoredPage) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("page store lock poisoned"))?;

        serde_json::to_writer(&mut *file, page).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.flush()
    }

    pub fn read_all(path: &Path) -> io::Result<Vec<StoredPage>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let reader = BufReader::new(file);
        let mut pages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let page = serde_json::from_str(&line)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            pages.push(page);
        }

        Ok(pages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn page(url: &str, title: Option<&str>, content: &str) -> StoredPage {
        StoredPage {
            url: url.to_string(),
            title: title.map(str::to_string),
            content: content.to_string(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pages.jsonl");
        let store = PageStore::open(path.clone()).unwrap();
        let pages = vec![
            page("https://example.com/one", Some("One"), "first page"),
            page("https://example.com/two", None, "second page"),
        ];

        store.append(&pages[0]).unwrap();
        store.append(&pages[1]).unwrap();

        assert_eq!(PageStore::read_all(&path).unwrap(), pages);
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonl");

        assert_eq!(PageStore::read_all(&path).unwrap(), Vec::new());
    }

    #[test]
    fn append_flushes_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pages.jsonl");
        let store = PageStore::open(path.clone()).unwrap();
        let stored_page = page("https://example.com", Some("Example"), "content");

        store.append(&stored_page).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<StoredPage>(contents.trim()).unwrap(),
            stored_page
        );
    }
}
