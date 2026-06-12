//! Tiny rotating file logger. No dependencies, lock per line, never panics.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

struct Logger {
    file: Option<File>,
    path: PathBuf,
    written: u64,
    echo_console: bool,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

pub fn init(path: PathBuf, echo_console: bool) {
    let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let _ = LOGGER.set(Mutex::new(Logger { file, path, written, echo_console }));
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

pub fn write(args: std::fmt::Arguments) {
    let Some(lock) = LOGGER.get() else { return };
    let Ok(mut lg) = lock.lock() else { return };
    let line = format!("[{}] {}\n", timestamp(), args);
    if lg.echo_console {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }
    if lg.written > MAX_LOG_BYTES {
        let old = lg.path.with_extension("old.log");
        lg.file = None;
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&lg.path, &old);
        lg.file = OpenOptions::new().create(true).append(true).open(&lg.path).ok();
        lg.written = 0;
    }
    if let Some(f) = lg.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
        lg.written += line.len() as u64;
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {
        $crate::logging::write(format_args!($($t)*))
    };
}
