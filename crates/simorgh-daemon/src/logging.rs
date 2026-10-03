//! Size-capped file logging, mirroring the mobile contract's `init`
//! (`ZeroNet-Mobile/docs/native-contract.md`, "Runtime notes"): the log goes
//! to `dataDir/simorgh.log`, rotated to `simorgh.log.1` past 2 MB, and the
//! level — taken from `SIMORGH_LOG` at start-up — can be changed at runtime
//! through `/rpc/set_log_level`.
//!
//! The implementation follows `crates/zray-mobile/src/android.rs`'s logging
//! module minus the logcat layer: a reloadable `LevelFilter` in front of a
//! plain fmt layer writing into a [`CappedFile`]. Stdout is deliberately not
//! used: the daemon's stdout carries exactly one line, the `SIMORGH_READY`
//! handshake the GUI parses.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{reload, Registry};

/// Rotate `simorgh.log` to `simorgh.log.1` past this size, so the two files
/// never hold more than twice it.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

static LEVEL: OnceLock<reload::Handle<LevelFilter, Registry>> = OnceLock::new();

/// `off|error|warn|info|debug|trace`, as in the contract's `init`.
pub fn parse_level(text: &str) -> Result<LevelFilter, String> {
    Ok(match text.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => LevelFilter::OFF,
        "error" => LevelFilter::ERROR,
        "warn" | "warning" => LevelFilter::WARN,
        "info" => LevelFilter::INFO,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        other => return Err(format!("unknown log level {other:?}")),
    })
}

/// A log file that rotates itself once it passes [`MAX_LOG_BYTES`].
pub struct CappedFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    cap: u64,
}

impl CappedFile {
    pub fn open(path: &Path, cap: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            written,
            cap,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        let _ = std::fs::rename(&self.path, PathBuf::from(rotated));
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?,
        );
        self.written = 0;
        Ok(())
    }
}

impl Write for CappedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.cap {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is closed"))?;
        file.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Change the level of the installed subscriber.
pub fn set_level(level: LevelFilter) -> Result<(), String> {
    LEVEL
        .get()
        .ok_or_else(|| "logging is not initialised".to_string())?
        .modify(|filter| *filter = level)
        .map_err(|error| format!("could not change the log level: {error}"))
}

/// Install the subscriber once per process; later calls only change the level.
pub fn init(data_dir: &Path, level: LevelFilter) -> Result<(), String> {
    if let Some(handle) = LEVEL.get() {
        return handle
            .modify(|filter| *filter = level)
            .map_err(|error| format!("could not change the log level: {error}"));
    }
    let file = CappedFile::open(&data_dir.join("simorgh.log"), MAX_LOG_BYTES)
        .map_err(|error| format!("could not open the log file: {error}"))?;
    let (filter, handle) = reload::Layer::new(level);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_thread_names(true)
        .with_writer(Mutex::new(file));
    let subscriber = Registry::default().with(filter).with(file_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| format!("a logger is already installed: {error}"))?;
    let _ = LEVEL.set(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn levels_parse_and_unknown_ones_are_rejected() {
        assert_eq!(parse_level("warn").unwrap(), LevelFilter::WARN);
        assert_eq!(parse_level("OFF").unwrap(), LevelFilter::OFF);
        assert!(parse_level("loud").is_err());
    }

    #[test]
    fn the_log_file_rotates_at_its_cap() {
        let dir = std::env::temp_dir().join(format!("simorgh-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("simorgh.log");
        let mut file = CappedFile::open(&path, 100).unwrap();
        for _ in 0..30 {
            file.write_all(b"0123456789\n").unwrap();
        }
        file.flush().unwrap();
        let current = std::fs::metadata(&path).unwrap().len();
        let rotated = std::fs::metadata(dir.join("simorgh.log.1")).unwrap().len();
        assert!(current <= 100, "{current}");
        assert!(rotated <= 100 && rotated > 0, "{rotated}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
