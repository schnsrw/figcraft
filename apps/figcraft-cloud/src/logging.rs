//! The cloud process's logger: `log` records (the relay's warnings and errors, request logs of
//! dependencies) go to stderr, one line each, filtered by `RUST_LOG`.
//!
//! `RUST_LOG` is a comma-separated list of directives: a bare level (`info`) sets the default and
//! `target=level` (`figcraft_cloud_relay=debug,sqlx=warn`) sets one crate or module path prefix.
//! Levels are `off`, `error`, `warn`, `info`, `debug`, `trace`; the default is `info`. An
//! unreadable directive is ignored (and said so on stderr), never fatal.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    default: LevelFilter,
    /// (target prefix, level), the longest prefix first.
    targets: Vec<(String, LevelFilter)>,
}

impl Logger {
    fn parse(spec: &str) -> (Self, Vec<String>) {
        let mut default = LevelFilter::Info;
        let mut targets = Vec::new();
        let mut ignored = Vec::new();
        for directive in spec.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            match directive.split_once('=') {
                Some((target, level)) => match level.trim().parse::<LevelFilter>() {
                    Ok(level) if !target.trim().is_empty() => targets.push((target.trim().to_string(), level)),
                    _ => ignored.push(directive.to_string()),
                },
                None => match directive.parse::<LevelFilter>() {
                    Ok(level) => default = level,
                    Err(_) => ignored.push(directive.to_string()),
                },
            }
        }
        targets.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        (Self { default, targets }, ignored)
    }

    fn level_for(&self, target: &str) -> LevelFilter {
        self.targets
            .iter()
            .find(|(prefix, _)| target == prefix || target.strip_prefix(prefix.as_str()).is_some_and(|rest| rest.starts_with("::")))
            .map_or(self.default, |(_, level)| *level)
    }

    /// The most any target logs: what `log::set_max_level` must allow.
    fn most(&self) -> LevelFilter {
        self.targets.iter().map(|(_, level)| *level).chain([self.default]).max().unwrap_or(LevelFilter::Off)
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level_for(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        // One write call per line so lines from different threads don't interleave.
        let line = format!("{} {level:<5} {}: {}\n", timestamp(SystemTime::now()), record.target(), record.args());
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// `2026-10-18T12:34:56Z` (UTC) without a date library.
fn timestamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Install the logger from `RUST_LOG` (default `info`). Called once at startup.
pub(crate) fn init() {
    let spec = std::env::var("RUST_LOG").unwrap_or_default();
    let (logger, ignored) = Logger::parse(&spec);
    let most = logger.most();
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(most);
    }
    for directive in ignored {
        eprintln!("figcraft-cloud: ignoring the RUST_LOG directive `{directive}`");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn level(spec: &str, target: &str) -> LevelFilter {
        Logger::parse(spec).0.level_for(target)
    }

    #[test]
    fn the_default_is_info_and_a_bare_level_replaces_it() {
        assert_eq!(level("", "anything"), LevelFilter::Info);
        assert_eq!(level("warn", "anything"), LevelFilter::Warn);
        assert_eq!(level("off", "anything"), LevelFilter::Off);
        assert_eq!(level("DEBUG", "anything"), LevelFilter::Debug, "levels are case-insensitive");
    }

    #[test]
    fn targets_match_by_module_path_prefix_longest_first() {
        let spec = "warn,figcraft_cloud_relay=debug,figcraft_cloud_relay::collab_backend=trace,sqlx=off";
        assert_eq!(level(spec, "figcraft_cloud_relay"), LevelFilter::Debug);
        assert_eq!(level(spec, "figcraft_cloud_relay::admission"), LevelFilter::Debug);
        assert_eq!(level(spec, "figcraft_cloud_relay::collab_backend"), LevelFilter::Trace);
        assert_eq!(level(spec, "sqlx::query"), LevelFilter::Off);
        // A longer crate name sharing a prefix is not the same target.
        assert_eq!(level(spec, "figcraft_cloud_relay_extra"), LevelFilter::Warn);
        assert_eq!(level(spec, "axum"), LevelFilter::Warn);
    }

    #[test]
    fn unreadable_directives_are_ignored_and_reported_not_fatal() {
        let (logger, ignored) = Logger::parse("loud,figcraft=chatty,=info,warn");
        assert_eq!(ignored, vec!["loud", "figcraft=chatty", "=info"]);
        assert_eq!(logger.level_for("x"), LevelFilter::Warn);
    }

    #[test]
    fn the_global_maximum_allows_the_most_verbose_target() {
        assert_eq!(Logger::parse("warn,figcraft_cloud_relay=trace").0.most(), LevelFilter::Trace);
        assert_eq!(Logger::parse("off").0.most(), LevelFilter::Off);
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        let at = |secs| timestamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400 + 86_399), "2000-02-29T23:59:59Z", "a leap day");
        assert_eq!(at(1792326896), "2026-10-18T12:34:56Z");
    }
}
