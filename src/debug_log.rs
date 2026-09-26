//! 可选的文件错误日志，用于排查代理、协议和配置故障。
//!
//! 开关立即生效；关闭时过滤所有事件，并释放文件、队列和后台线程。
//! 仅记录 ERROR，按本地日期滚动，写入 `./log/debug-YYYYMMDD.log`。

use anyhow::{Context, Result, anyhow};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    Registry,
    filter::LevelFilter,
    fmt::{self, format::Writer, time::FormatTime},
    layer::SubscriberExt,
    reload,
    util::SubscriberInitExt,
};

/// 生命周期与应用一致；关闭时仅保留轻量的订阅器和开关状态。
pub struct DebugLog {
    dir: PathBuf,
    filter: reload::Handle<LevelFilter, Registry>,
    writer: SwitchWriter,
    // 同时串行化 UI 与配置重载对开关的修改。
    worker: Mutex<Option<WorkerGuard>>,
}

impl DebugLog {
    /// 安装初始为 OFF 的订阅器；此时不创建目录、文件或线程。
    pub fn new(dir: &Path) -> Result<Self> {
        let (subscriber, log) = subscriber(dir);
        subscriber
            .try_init()
            .map_err(|error| anyhow!("无法安装日志订阅器：{error}"))?;
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // tracing 在 OFF 时不会求值字段，也就不会捕获回溯。
            tracing::error!(
                target: "majsoul_max_rs::panic",
                version = env!("CARGO_PKG_VERSION"),
                panic = %info,
                backtrace = %std::backtrace::Backtrace::force_capture(),
                "线程发生 panic"
            );
            previous_hook(info);
        }));
        Ok(log)
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if worker.is_some() == enabled {
            return Ok(());
        }
        if enabled {
            let file = DailyFile::open(&self.dir)
                .with_context(|| format!("无法创建日志文件于 {}", self.dir.display()))?;
            let (writer, guard) = NonBlockingBuilder::default().lossy(false).finish(file);
            *self
                .writer
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(writer);
            if let Err(error) = self.filter.reload(LevelFilter::ERROR) {
                drop(
                    self.writer
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take(),
                );
                drop(guard);
                return Err(anyhow!("无法开启日志过滤器：{error}"));
            }
            *worker = Some(guard);
        } else {
            // 先关闭调用点，再断开 writer，最后等待队列刷盘和线程退出。
            self.filter
                .reload(LevelFilter::OFF)
                .map_err(|error| anyhow!("无法关闭日志过滤器：{error}"))?;
            drop(
                self.writer
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take(),
            );
            drop(worker.take());
        }
        Ok(())
    }

    pub fn is_enabled(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
}

impl Drop for DebugLog {
    fn drop(&mut self) {
        let _ = self.set_enabled(false);
    }
}

#[derive(Clone, Default)]
struct SwitchWriter(Arc<Mutex<Option<NonBlocking>>>);

impl Write for SwitchWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut writer = self.0.lock().unwrap_or_else(|error| error.into_inner());
        match writer.as_mut() {
            Some(writer) => writer.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut writer = self.0.lock().unwrap_or_else(|error| error.into_inner());
        match writer.as_mut() {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }
}

pub(crate) fn subscriber(
    dir: &Path,
) -> (
    impl tracing::Subscriber + Send + Sync + 'static + use<>,
    DebugLog,
) {
    let writer = SwitchWriter::default();
    let log_writer = writer.clone();
    let (filter, handle) = reload::Layer::new(LevelFilter::OFF);
    let subscriber = tracing_subscriber::registry()
        // 仅在 OFF / ERROR 间切换，RUST_LOG 不能将正常流量写入文件。
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(move || writer.clone())
                .with_timer(LocalTime)
                .with_ansi(false)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true),
        );
    let log = DebugLog {
        dir: dir.to_path_buf(),
        filter: handle,
        writer: log_writer,
        worker: Mutex::new(None),
    };
    (subscriber, log)
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
