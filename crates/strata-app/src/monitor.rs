//! # Resource monitor: CPU / RAM of this process + workspace data size
//!
//! The bottom status bar shows *consumed resources* ("сколько ест Strata").
//! On Linux everything is read from the `/proc` pseudo-filesystem — no extra
//! dependencies:
//!
//! * process CPU — deltas of `utime + stime` in `/proc/self/stat`, divided by
//!   wall time (CPU clock ticks are 100/sec on Linux, `USER_HZ`);
//! * process RAM — `VmRSS` in `/proc/self/status` (resident set size);
//! * system memory — `MemTotal` in `/proc/meminfo`;
//! * workspace data size — recursive walk of `data/` (computed only when the
//!   workspace directory changes, see [`Sampler`]).
//!
//! ## Threading model
//!
//! [`Sampler`] is driven from a plain `std::thread` (one sample per second)
//! that sends [`Sample`]s into a Dioxus coroutine — see `main.rs`. It never
//! touches Dioxus state itself.
//!
//! ## Non-Linux platforms
//!
//! Every reader returns `None` there; the UI renders "—". (The code stays
//! compile-clean without pulling in a cross-platform crate such as `sysinfo`.)

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One second between samples — the status bar tick.
pub const TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// One snapshot of consumed resources, ready to render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Process CPU usage in percent, smoothed over the last interval.
    /// `f32::NAN` means "cannot measure on this platform".
    pub cpu_pct: Option<f32>,
    /// Resident set size of this process (bytes).
    pub rss_bytes: Option<u64>,
    /// Total system memory (bytes).
    pub mem_total_bytes: Option<u64>,
    /// Total size of the workspace `data/` folder (bytes), `None` if no
    /// workspace is open (or it cannot be measured).
    pub data_bytes: Option<u64>,
}

impl Sample {
    /// An "unknown" sample — shown as `—` in the status bar.
    pub const UNKNOWN: Sample = Sample {
        cpu_pct: None,
        rss_bytes: None,
        mem_total_bytes: None,
        data_bytes: None,
    };
}

impl Default for Sample {
    fn default() -> Self {
        Sample::UNKNOWN
    }
}

/// Stateful sampler: CPU percent needs the *previous* reading to compute a
/// delta, and the data-dir size must only be re-walked when the workspace
/// changes (a walk reads every file's metadata — not free on big datasets).
pub struct Sampler {
    /// Previous (cpu ticks, wall instant) — for the CPU delta.
    prev: Option<(u64, Instant)>,
    /// Which `data/` dir we already measured.
    last_data_dir: Option<PathBuf>,
    /// The measured size of `last_data_dir`.
    last_data_bytes: Option<u64>,
}

impl Sampler {
    /// Start a fresh sampler (first `sample()` reports CPU 0% by design —
    /// there is no "previous tick" yet).
    pub fn new() -> Self {
        Sampler {
            prev: None,
            last_data_dir: None,
            last_data_bytes: None,
        }
    }

    /// Produce one sample, optionally given the current workspace `data/`
    /// directory to measure.
    pub fn sample(&mut self, data_dir: Option<&Path>) -> Sample {
        let (now_ticks, now) = (self.process_cpu_ticks(), Instant::now());
        let cpu_pct = match (self.prev, now_ticks) {
            // Need two readings at least one wall-interval apart.
            (Some((prev_ticks, prev_at)), Some(now_ticks))
                if now_ticks >= prev_ticks && now > prev_at =>
            {
                let delta_ticks = now_ticks - prev_ticks;
                let wall_secs = now.duration_since(prev_at).as_secs_f64();
                // ticks are 1/100 s each; percent = (cpu_secs / wall_secs) * 100.
                let cpu_secs = delta_ticks as f64 / CPU_TICKS_PER_SEC as f64;
                Some(((cpu_secs / wall_secs) * 100.0) as f32)
            }
            _ => None,
        };
        self.prev = now_ticks.map(|ticks| (ticks, now));

        // Re-walk the data dir when the path changed — or while a previous
        // walk failed (e.g. the folder did not exist yet) so we retry.
        let changed = match (&self.last_data_dir, data_dir) {
            (Some(last), Some(next)) => last != next,
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (None, None) => false,
        };
        let retry_failed = data_dir.is_some()
            && data_dir == self.last_data_dir.as_deref()
            && self.last_data_bytes.is_none();
        if changed || retry_failed {
            // A failed walk (folder deleted meanwhile, permissions, …) simply
            // leaves the size unknown; the status bar shows "—".
            self.last_data_bytes = data_dir.and_then(|dir| dir_total_bytes(dir).ok());
            self.last_data_dir = data_dir.map(Path::to_path_buf);
        }

        Sample {
            cpu_pct,
            rss_bytes: self.process_rss_bytes(),
            mem_total_bytes: system_mem_total_bytes(),
            data_bytes: if data_dir.is_some() {
                self.last_data_bytes
            } else {
                None
            },
        }
    }

    /// Total jiffies (utime + stime) this process has used on the CPU.
    fn process_cpu_ticks(&self) -> Option<u64> {
        read_self_stat()
            .and_then(|text| parse_self_stat_cpu_ticks(&text))
            .or_else(fallback_cpu_ticks)
    }

    /// Resident set size of this process (bytes).
    fn process_rss_bytes(&self) -> Option<u64> {
        read_self_status()
            .and_then(|text| parse_status_kb(&text, "VmRSS"))
            .map(|kb| kb.saturating_mul(1024))
            .or_else(fallback_rss_bytes)
    }
}

/// How many CPU clock ticks make one second (Linux `USER_HZ`, standard = 100).
const CPU_TICKS_PER_SEC: u64 = 100;

// ---------------------------------------------------------------------------
// Linux readers (/proc). Compiled on Unix; other platforms get the fallbacks.
// ---------------------------------------------------------------------------

/// `/proc/self/stat` as a string (or `None` if unreadable).
#[cfg(target_os = "linux")]
fn read_self_stat() -> Option<String> {
    std::fs::read_to_string("/proc/self/stat").ok()
}

/// `/proc/self/status` as a string (or `None` if unreadable).
#[cfg(target_os = "linux")]
fn read_self_status() -> Option<String> {
    std::fs::read_to_string("/proc/self/status").ok()
}

/// Parse the total CPU ticks (`utime` + `stime`, fields 14+15 of `/proc` stat)
/// from the raw stat text. The command name in field 2 may contain spaces and
/// `)` characters, so we split at the *last* `)` instead of splitting on
/// whitespace blindly.
#[cfg(target_os = "linux")]
fn parse_self_stat_cpu_ticks(text: &str) -> Option<u64> {
    let after_comm = text.rsplit_once(')')?.1.trim_start();
    // After the command name the fields restart at #3 (state). Field #14
    // (utime) is therefore token #11, field #15 (stime) token #12.
    let mut tokens = after_comm.split_whitespace();
    let utime: u64 = tokens.nth(11)?.parse().ok()?;
    let stime: u64 = tokens.next()?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

/// Parse a `NNNN kB` value for `key` from a `/proc/*/status`-style text.
#[cfg(target_os = "linux")]
fn parse_status_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        value.split_whitespace().next()?.parse().ok()
    })
}

/// System total memory from `/proc/meminfo` (`MemTotal` in kB).
#[cfg(target_os = "linux")]
fn system_mem_total_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_status_kb(&text, "MemTotal").map(|kb| kb.saturating_mul(1024))
}

// ---------------------------------------------------------------------------
// Fallbacks for non-Linux platforms — we cannot measure, report "unknown".
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn read_self_stat() -> Option<String> {
    None
}

#[cfg(not(target_os = "linux"))]
fn read_self_status() -> Option<String> {
    None
}

#[cfg(not(target_os = "linux"))]
fn parse_self_stat_cpu_ticks(_text: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn parse_status_kb(_text: &str, _key: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn system_mem_total_bytes() -> Option<u64> {
    None
}

/// Cross-platform "cannot measure" placeholder for the CPU reading.
fn fallback_cpu_ticks() -> Option<u64> {
    None
}

/// Cross-platform "cannot measure" placeholder for the RSS reading.
fn fallback_rss_bytes() -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Data dir walk (platform independent)
// ---------------------------------------------------------------------------

/// Recursively sum the size of every regular file under `dir`.
pub fn dir_total_bytes(dir: &Path) -> io::Result<u64> {
    fn walk(path: &Path, acc: &mut u64) -> io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(&entry.path(), acc)?;
            } else if file_type.is_file() {
                *acc = acc.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(())
    }
    let mut total = 0u64;
    walk(dir, &mut total)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_ticks_parse_comm_with_spaces_and_parens() {
        // Command "(my proc) v2" contains spaces and a ')' — parse must look
        // at the *last* ')'. Sample numbers below: utime=100, stime=20 → 120.
        let text = "1234 (my proc) v2) S 0 1 1 0 -1 4194560 42 0 0 0 \
                    100 20 0 0 20 0 1 0 1000 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 \
                    0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_self_stat_cpu_ticks(text), Some(120));
    }

    #[test]
    fn stat_parse_returns_none_on_garbage() {
        assert_eq!(parse_self_stat_cpu_ticks(""), None);
        assert_eq!(parse_self_stat_cpu_ticks("1234 (x) R"), None);
    }

    #[test]
    fn status_kb_value_is_found_and_parsed() {
        let text = "Name:\tstrata-app\nVmRSS:\t  41234 kB\nVmSize:\t999999 kB\n";
        assert_eq!(parse_status_kb(text, "VmRSS"), Some(41234));
        assert_eq!(parse_status_kb(text, "VmSize"), Some(999999));
        assert_eq!(parse_status_kb(text, "Missing"), None);
    }

    #[test]
    fn sampler_reports_unknown_when_readers_fail() {
        // No /proc available in this unit-test context on non-Linux; even on
        // Linux an empty state has no CPU delta yet → cpu_pct is None.
        let mut sampler = Sampler::new();
        let sample = sampler.sample(None);
        assert_eq!(sample.cpu_pct, None);
    }

    #[test]
    fn dir_total_bytes_sums_files_recursively() {
        let dir = std::env::temp_dir().join(format!("strata_mon_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "12345").unwrap(); // 5 bytes
        std::fs::write(dir.join("sub/b.txt"), "1234567890").unwrap(); // 10 bytes
        assert_eq!(dir_total_bytes(&dir).unwrap(), 15);
        let _ = std::fs::remove_dir_all(dir);
    }
}
