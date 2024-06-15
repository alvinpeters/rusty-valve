use std::env::VarError;
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use anyhow::{anyhow, Context, Result};
use time::format_description::well_known::{Rfc2822, Rfc3339};
use tracing::dispatcher::get_default;
use tracing::field::debug;
use tracing::instrument::WithSubscriber;
use tracing::Level;
use tracing::level_filters::ParseLevelFilterError;
use tracing::metadata::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{filter, fmt, Layer as LayerTrait, Registry};
use tracing_subscriber::fmt::format::{Format, Pretty};
use tracing_subscriber::fmt::Layer;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use crate::simple_error;

pub(crate) mod simple {
    #[macro_export]
    macro_rules! simple_info {
        () => { simple_info!("") };
        ($($arg:tt)+) => { println!("INFO: {}", format_args!($($arg)+)) };
    }

    #[macro_export]
    macro_rules! simple_warn {
        () => { simple_warn!("") };
        ($($arg:tt)+) => { eprintln!("WARN: {}", format_args!($($arg)+)) };
    }

    #[macro_export]
    macro_rules! simple_error {
        () => { simple_error!() };
        ($($arg:tt)+) => {
            eprintln!("ERROR: {}", format_args!($($arg)+))
        };
    }
}

pub(crate) struct LoggerConfig {
    pub(crate) print_level: LevelFilter,
    pub(crate) logfile_level: LevelFilter,
    pub(crate) stdout_log: Option<bool>,
    pub(crate) log_path: Option<PathBuf>,
}

pub(crate) struct LoggerGuard {
    stdout_guard: Option<WorkerGuard>,
    stderr_guard: Option<WorkerGuard>,
    logfile_guard: Option<WorkerGuard>,
}

impl Drop for LoggerGuard {
    fn drop(&mut self) {
        if let Some(g) = self.stdout_guard.take() {
            drop(g);
        }
        if let Some(g) = self.logfile_guard.take() {
            drop(g);
        }
    }
}

impl Default for LoggerConfig {

    fn default() -> Self {
        let level = std::env::var("RUSTY_VALVE_LOG_LEVEL").ok()
            .map(|s| LevelFilter::from_str(s.as_str())).and_then(|sr| sr.ok())
            .unwrap_or(
                if cfg!(debug_assertions) {
                    LevelFilter::TRACE
                } else {
                    LevelFilter::INFO
                }
            );
        LoggerConfig {
            print_level: level,
            logfile_level: level,
            stdout_log: None,
            log_path: None,
        }
    }
}

pub(crate) fn init_logger(config: LoggerConfig) -> Result<LoggerGuard> {
    // Logger guard to block drop
    let mut logger_guard = LoggerGuard {
        stdout_guard: None,
        stderr_guard: None,
        logfile_guard: None,
    };
    //let mut main_layer = tracing_subscriber::fmt::layer();
    let stdout_stderr_layer = config.stdout_log.unwrap_or(config.log_path.is_none()).then(|| {
        let time_offset =
            time::UtcOffset::current_local_offset().unwrap_or_else(|_| time::UtcOffset::UTC);
        let timer = fmt::time::OffsetTime::new(time_offset, Rfc2822);
        let (stdout_non_blocking, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());
        let (stderr_non_blocking, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());
        logger_guard.stdout_guard = Option::from(stdout_guard);
        logger_guard.stderr_guard = Option::from(stderr_guard);
        #[cfg(debug_assertions)]
        let stdout_layer =  tracing_subscriber::fmt::layer()
            .pretty()
            .with_timer(timer.clone())
            .with_writer(stdout_non_blocking.with_min_level(Level::INFO));
        #[cfg(debug_assertions)]
        let stderr_layer =  tracing_subscriber::fmt::layer()
            .pretty()
            .with_timer(timer.clone())
            .with_writer(stderr_non_blocking.with_max_level(Level::WARN));
        #[cfg(not(debug_assertions))]
        let stdout_layer =  tracing_subscriber::fmt::layer()
            .compact()
            .with_timer(timer.clone())
            .with_writer(stdout_non_blocking.with_min_level(Level::INFO));
        #[cfg(not(debug_assertions))]
        let stderr_layer =  tracing_subscriber::fmt::layer()
            .compact()
            .with_timer(timer.clone())
            .with_writer(stderr_non_blocking.with_max_level(Level::WARN));

        stdout_layer.and_then(stderr_layer)
    });
    // A layer that logs events to a file.
    let logfile_layer = config.log_path.and_then(|mut path| {
        let fa = if path.is_dir() {
            rolling::daily(path, "log")
        } else {
            let basename = path.file_name()
                .ok_or(|| anyhow!("couldn't get basename from: {}", path.display())).ok()?;
            let dirname = path.parent()
                .ok_or(|| anyhow!("couldn't get dirname from: {}", path.display())).ok()?;
            rolling::never(dirname, basename)
        };
        let (non_blocking_appender, guard) = tracing_appender::non_blocking(fa);
        logger_guard.logfile_guard = Option::from(guard);
        let layer = tracing_subscriber::fmt::layer().compact().with_ansi(false)
            .with_writer(non_blocking_appender);
        Some(layer)
    });

    tracing_subscriber::registry()
        .with(logfile_layer.with_filter(config.logfile_level))
        .with(stdout_stderr_layer.with_filter(config.print_level))
        .init();
    Ok(logger_guard)
}
