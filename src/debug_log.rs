//! 可选的文件调试日志，用于排查解锁资源不生效等问题。
//!
//! 只有 `settings.json` 的 `debugLog` 为 true 时才会调用 [`start`] 安装订阅器；
//! 未安装时所有 tracing 宏只做一次原子读取即返回。
//! 日志按本地日期滚动，写入 `<配置目录>/logs/debug-YYYYMMDD.log`。

use anyhow::{Context, Result, anyhow};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::Writer, time::FormatTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

/// 本 crate 记录 debug 及以上；hudsucker 等依赖只记录 warn 及以上
/// （TLS 握手失败、上游连接失败等）。设置 `RUST_LOG` 可覆盖。
const DEFAULT_FILTER: &str = "warn,majsoul_max_rs=debug";

/// 安装文件日志订阅器。返回的 guard 必须存活到程序退出，drop 时会把缓冲写完。
pub fn start(dir: &Path) -> Result<WorkerGuard> {
    let file = DailyFile::open(dir)
        .with_context(|| format!("无法创建日志文件于 {}", dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_timer(LocalTime))
        .try_init()
        .map_err(|error| anyhow!("无法安装日志订阅器：{error}"))?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_is_created_and_appended_across_days() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "majsoul-max-log-{}-{unique}",
            std::process::id()
        ));
        let mut file = DailyFile::open(&dir).unwrap();
        file.write_all(b"first\n").unwrap();
        file.day = "19990101".into();
        file.write_all(b"second\n").unwrap();
        drop(file);

        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(files.iter().any(|name| name == "debug-19990101.log"));
        assert!(files.iter().any(|name| name.starts_with("debug-20")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

/// 由 tracing-appender 的后台线程独占写入，因此不需要额外加锁。
struct DailyFile {
    dir: PathBuf,
    day: String,
    file: File,
}

impl DailyFile {
    fn open(dir: &Path) -> io::Result<Self> {
        let day = today();
        let file = open_log_file(dir, &day)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            day,
            file,
        })
    }
}

impl Write for DailyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let day = today();
        if day != self.day {
            self.file = open_log_file(&self.dir, &day)?;
            self.day = day;
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y%m%d").to_string()
}

fn open_log_file(dir: &Path, day: &str) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("debug-{day}.log")))
}
