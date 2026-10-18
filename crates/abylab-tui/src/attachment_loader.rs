//! Bounded, cancellable attachment IO, independent of the UI event loop.
use std::{
    io::{self, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
const CAPACITY: usize = 8;
const CLIP_TIMEOUT: Duration = Duration::from_secs(3);

pub enum Source {
    File(PathBuf),
    Clipboard,
}
pub struct Loaded {
    pub name: String,
    pub path: String,
    pub media_type: String,
    pub data: Vec<u8>,
    pub caption: String,
}
struct Job {
    generation: u64,
    source: Source,
    caption: String,
}
struct Completion {
    generation: u64,
    result: Result<Loaded, String>,
}

/// Owns request capacity, cancellation and draft correlation. Results may be
/// rebased only after the UI itself inserts a completed image into that draft.
pub struct Loader {
    tx: mpsc::SyncSender<Job>,
    rx: mpsc::Receiver<Completion>,
    generation: Arc<AtomicU64>,
    key: (String, u64),
    pending: usize,
}
impl Loader {
    pub fn new() -> io::Result<Self> {
        let (tx, jobs) = mpsc::sync_channel::<Job>(CAPACITY);
        let (results, rx) = mpsc::sync_channel(1);
        let generation = Arc::new(AtomicU64::new(1));
        let worker_generation = Arc::clone(&generation);
        std::thread::Builder::new()
            .name("aby-attachments".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let cancelled = || worker_generation.load(Ordering::Acquire) != job.generation;
                    if cancelled() {
                        continue;
                    }
                    let result = load(job.source, job.caption, &cancelled);
                    if !cancelled()
                        && results
                            .send(Completion {
                                generation: job.generation,
                                result,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            rx,
            generation,
            key: (String::new(), 0),
            pending: 0,
        })
    }
    pub fn sync(&mut self, session: &str, revision: u64) {
        if self.key.0 != session || self.key.1 != revision {
            self.cancel();
            self.key = (session.into(), revision);
        }
    }
    pub fn rebase(&mut self, revision: u64) {
        self.key.1 = revision;
    }
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending = 0;
        while self.rx.try_recv().is_ok() {}
    }
    pub fn submit(&mut self, source: Source, caption: String) -> Result<(), String> {
        if self.pending >= CAPACITY {
            return Err("too many images loading (limit 8)".into());
        }
        self.tx
            .try_send(Job {
                generation: self.generation.load(Ordering::Acquire),
                source,
                caption,
            })
            .map_err(|_| "image loader is busy or unavailable".to_string())?;
        self.pending += 1;
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Result<Loaded, String>> {
        while let Ok(completion) = self.rx.try_recv() {
            if completion.generation == self.generation.load(Ordering::Acquire) {
                self.pending = self.pending.saturating_sub(1);
                return Some(completion.result);
            }
        }
        None
    }
}
impl Drop for Loader {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn media_type_for(path: &str) -> Option<&'static str> {
    match path.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}
fn check(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("image loading cancelled".into())
    } else {
        Ok(())
    }
}
fn read_file(path: &std::path::Path, cancelled: &impl Fn() -> bool) -> Result<Vec<u8>, String> {
    check(cancelled)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO may block in open before metadata can be checked.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(not(unix))]
    if !std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("image path must be a regular file".into());
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("image path must be a regular file".into());
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err("image exceeds the 32 MiB limit".into());
    }
    read_bounded(&mut file, cancelled)
}
fn read_bounded(reader: &mut impl Read, cancelled: &impl Fn() -> bool) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buf = [0; 64 * 1024];
    loop {
        check(cancelled)?;
        let count = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > MAX_BYTES {
            return Err("image exceeds the 32 MiB limit".into());
        }
        bytes.extend_from_slice(&buf[..count]);
    }
}
fn load(source: Source, caption: String, cancelled: &impl Fn() -> bool) -> Result<Loaded, String> {
    let (path, name, media_type, data) = match source {
        Source::File(path) => {
            let media_type = media_type_for(&path.to_string_lossy())
                .ok_or("unsupported image — use .png .jpg .jpeg .webp .gif")?;
            let data = read_file(&path, cancelled)?;
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let stored = std::fs::canonicalize(&path)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            (stored, name, media_type.to_string(), data)
        }
        Source::Clipboard => (
            "clipboard".into(),
            "clipboard.png".into(),
            "image/png".into(),
            clipboard(cancelled)?,
        ),
    };
    check(cancelled)?;
    Ok(Loaded {
        path,
        name,
        media_type,
        data,
        caption,
    })
}

#[cfg(unix)]
fn command_bytes(
    command: &mut std::process::Command,
    cancelled: &impl Fn() -> bool,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use std::{
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, Stdio},
    };
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            // Kill descendants that may have inherited the stdout pipe too.
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    check(cancelled)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut process = Process(command.spawn().map_err(|e| e.to_string())?);
    let mut stdout = process
        .0
        .stdout
        .take()
        .ok_or("clipboard stdout unavailable")?;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut buf = [0; 64 * 1024];
    let mut eof = false;
    loop {
        check(cancelled)?;
        if Instant::now() >= deadline {
            return Err("clipboard image command timed out".into());
        }
        if !eof {
            match stdout.read(&mut buf) {
                Ok(0) => eof = true,
                Ok(count) => {
                    if bytes.len() + count > MAX_BYTES {
                        return Err("clipboard image exceeds the 32 MiB limit".into());
                    }
                    bytes.extend_from_slice(&buf[..count]);
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        if eof {
            if let Some(status) = process.0.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(bytes)
                } else {
                    Err("clipboard has no image".into())
                };
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[cfg(target_os = "linux")]
fn clipboard(cancelled: &impl Fn() -> bool) -> Result<Vec<u8>, String> {
    let mut last_error = "clipboard has no image".to_string();
    for (name, args) in [
        ("wl-paste", vec!["--type", "image/png"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ] {
        check(cancelled)?;
        match command_bytes(
            std::process::Command::new(name).args(args),
            cancelled,
            CLIP_TIMEOUT,
        ) {
            Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
            Err(error) => last_error = error,
            _ => {}
        }
    }
    Err(last_error)
}
#[cfg(target_os = "macos")]
fn clipboard(cancelled: &impl Fn() -> bool) -> Result<Vec<u8>, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = directory.path().join("clipboard.png");
    let script = "on run argv\nset d to (the clipboard as «class PNGf»)\nset h to open for access (POSIX file (item 1 of argv)) with write permission\nwrite d to h as «class PNGf»\nclose access h\nend run";
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("osascript");
    command.arg("-e").arg(script).arg(&path);
    // The macOS bridge writes a file instead of stdout. Bound that file in
    // the child too, so the reader's cap is not merely a post-write check.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: MAX_BYTES as libc::rlim_t,
                rlim_max: MAX_BYTES as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    command_bytes(&mut command, cancelled, CLIP_TIMEOUT)?;
    read_file(&path, cancelled)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clipboard(_: &impl Fn() -> bool) -> Result<Vec<u8>, String> {
    Err("clipboard images are unsupported on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streamed_limit_rejects_growth_without_reading_the_whole_source() {
        struct Growing(usize);
        impl Read for Growing {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.0 += buffer.len();
                Ok(buffer.len())
            }
        }
        let mut reader = Growing(0);
        assert!(read_bounded(&mut reader, &|| false)
            .unwrap_err()
            .contains("32 MiB"));
        assert!(reader.0 <= MAX_BYTES + 64 * 1024);
    }
    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_without_waiting_for_a_writer() {
        use std::ffi::CString;
        let path = std::env::temp_dir().join(format!("aby-fifo-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        std::thread::spawn(move || {
            tx.send(read_file(&worker_path, &|| false)).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        std::fs::remove_file(path).unwrap();
        assert!(result.unwrap().unwrap_err().contains("regular file"));
    }
    #[cfg(unix)]
    #[test]
    fn clipboard_timeout_and_cancellation_reap_the_process() {
        let error = command_bytes(
            std::process::Command::new("sh")
                .args(["-c", "trap '' TERM; while :; do sleep 1; done"]),
            &|| false,
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
        let count = std::cell::Cell::new(0);
        let cancel = || {
            count.set(count.get() + 1);
            count.get() > 3
        };
        assert!(command_bytes(
            std::process::Command::new("sh").args(["-c", "sleep 60"]),
            &cancel,
            Duration::from_secs(2)
        )
        .unwrap_err()
        .contains("cancelled"));
    }
    #[test]
    fn stale_completions_are_discarded_and_capacity_is_bounded() {
        let (tx, _jobs) = mpsc::sync_channel(CAPACITY);
        let (results, rx) = mpsc::sync_channel(1);
        let generation = Arc::new(AtomicU64::new(1));
        let mut loader = Loader {
            tx,
            rx,
            generation,
            key: ("a".into(), 1),
            pending: 0,
        };
        for _ in 0..CAPACITY {
            loader.submit(Source::Clipboard, String::new()).unwrap();
        }
        assert!(loader.submit(Source::Clipboard, String::new()).is_err());
        loader.sync("b", 1);
        results
            .send(Completion {
                generation: 1,
                result: Err("old session".into()),
            })
            .unwrap();
        assert!(loader.poll().is_none());
        let current = loader.generation.load(Ordering::Acquire);
        loader.sync("b", 2);
        results
            .send(Completion {
                generation: current,
                result: Err("old draft".into()),
            })
            .unwrap();
        assert!(loader.poll().is_none());
    }
}
