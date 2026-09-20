use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex, OnceLock,
    },
};

#[derive(Clone)]
pub struct LogEvent {
    pub time: String,
    pub level: String,
    pub message: String,
}
struct Writer {
    file: File,
    bytes: u64,
}
pub struct AppLogger {
    disk: mpsc::Sender<DiskEvent>,
    directory: PathBuf,
    writer: Mutex<Writer>,
    events: Mutex<VecDeque<LogEvent>>,
}
static LOGGER: OnceLock<AppLogger> = OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(0);
enum DiskEvent {
    Line(String),
    Flush(mpsc::Sender<()>),
}
impl AppLogger {
    pub fn revision() -> u64 {
        REVISION.load(Ordering::Relaxed)
    }
    pub fn init(data: &Path, debug: bool) -> anyhow::Result<()> {
        let directory = data.join("Logs");
        fs::create_dir_all(&directory)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("maxi.log"))?;
        let bytes = file.metadata()?.len();
        let (disk, requests) = mpsc::channel();
        LOGGER
            .set(Self {
                disk,
                directory,
                writer: Mutex::new(Writer { file, bytes }),
                events: Mutex::new(VecDeque::with_capacity(500)),
            })
            .map_err(|_| anyhow::anyhow!("Logger already initialized"))?;
        log::set_logger(LOGGER.get().unwrap())?;
        std::thread::spawn(move || {
            let logger = LOGGER.get().unwrap();
            while let Ok(event) = requests.recv() {
                match event {
                    DiskEvent::Line(line) => {
                        if let Ok(mut writer) = logger.writer.lock() {
                            if writer.bytes + line.len() as u64 > 2 * 1024 * 1024 {
                                let _ = logger.rotate(&mut writer);
                            }
                            if writer.file.write_all(line.as_bytes()).is_ok() {
                                writer.bytes += line.len() as u64;
                            }
                            let _ = writer.file.flush();
                        }
                    }
                    DiskEvent::Flush(done) => {
                        if let Ok(mut writer) = logger.writer.lock() {
                            let _ = writer.file.flush();
                        }
                        let _ = done.send(());
                    }
                }
            }
        });
        Self::set_debug(debug);
        Ok(())
    }
    pub fn set_debug(debug: bool) {
        log::set_max_level(if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        });
    }
    pub fn events() -> Vec<LogEvent> {
        LOGGER
            .get()
            .map(|l| {
                l.events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .rev()
                    .take(200)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn clear_view() {
        if let Some(l) = LOGGER.get() {
            l.events.lock().unwrap_or_else(|p| p.into_inner()).clear();
            REVISION.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn rotate(&self, writer: &mut Writer) -> std::io::Result<()> {
        writer.file.flush()?;
        // Copy old files rather than renaming an open Windows file handle.
        for i in (1..=3).rev() {
            let old = self.directory.join(format!("maxi.{i}.log"));
            if old.exists() {
                fs::copy(old, self.directory.join(format!("maxi.{}.log", i + 1)))?;
            }
        }
        fs::copy(
            self.directory.join("maxi.log"),
            self.directory.join("maxi.1.log"),
        )?;
        writer.file.set_len(0)?;
        writer.bytes = 0;
        Ok(())
    }
}
impl Log for AppLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
            && (metadata.target().starts_with("maxi_sound_set")
                || metadata.level() <= log::Level::Warn)
    }
    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = Local::now();
        let message = record.args().to_string().replace(['\r', '\n'], " ");
        let line = format!(
            "{} [{:5}] {}: {}\n",
            now.format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            message
        );
        let _ = self.disk.send(DiskEvent::Line(line));
        if let Ok(mut events) = self.events.lock() {
            if events.len() >= 500 {
                events.pop_front();
            }
            events.push_back(LogEvent {
                time: now.format("%H:%M:%S").to_string(),
                level: record.level().to_string(),
                message,
            });
            REVISION.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn flush(&self) {
        let (done, waiting) = mpsc::channel();
        if self.disk.send(DiskEvent::Flush(done)).is_ok() {
            let _ = waiting.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}
