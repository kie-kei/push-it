use anyhow::Result;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

/// Per-app log sink: a file under $PUSHIT_DATA/logs/<app>.log plus a
/// broadcast channel for SSE streaming.
pub struct LogStore {
    dir: PathBuf,
    channels: parking_lot::Mutex<std::collections::HashMap<String, broadcast::Sender<String>>>,
}

impl LogStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(LogStore {
            dir,
            channels: Default::default(),
        })
    }

    fn path(&self, app: &str) -> PathBuf {
        self.dir.join(format!("{app}.log"))
    }

    fn sender(&self, app: &str) -> broadcast::Sender<String> {
        let mut m = self.channels.lock();
        m.entry(app.to_string())
            .or_insert_with(|| broadcast::channel(512).0)
            .clone()
    }

    pub fn subscribe(&self, app: &str) -> broadcast::Receiver<String> {
        self.sender(app).subscribe()
    }

    pub fn append(&self, app: &str, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(app))?;
        let tx = self.sender(app);
        for line in data.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(line).into_owned();
            let stamped = format!("[{}] {s}\n", now_hms());
            f.write_all(stamped.as_bytes())?;
            let _ = tx.send(stamped.trim_end().to_string());
        }
        Ok(())
    }

    pub fn tail(&self, app: &str, n: usize) -> Result<Vec<String>> {
        let p = self.path(app);
        if !p.exists() {
            return Ok(vec![]);
        }
        let mut dq: VecDeque<String> = VecDeque::with_capacity(n);
        for line in BufReader::new(std::fs::File::open(&p)?).lines() {
            let line = line?;
            if dq.len() == n {
                dq.pop_front();
            }
            dq.push_back(line);
        }
        Ok(dq.into_iter().collect())
    }

    /// New bytes appended since `offset`; returns (data, new_offset).
    pub fn read_from(&self, app: &str, offset: u64) -> Result<(Vec<u8>, u64)> {
        let p: &Path = &self.path(app);
        if !p.exists() {
            return Ok((vec![], offset));
        }
        let mut f = std::fs::File::open(p)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![];
        f.read_to_end(&mut buf)?;
        let new_off = offset + buf.len() as u64;
        Ok((buf, new_off))
    }
}

fn now_hms() -> String {
    // cheap timestamp without chrono
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let d = secs / 86400;
    let t = secs % 86400;
    format!("d{d} {:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}
