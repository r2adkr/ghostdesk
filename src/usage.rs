//! What agents have used, kept per day for the Usage tab.
//!
//! Agents running inside GhostDesk report real token counts from the AI service. Outside agents
//! (Claude Code, Codex, …) drive screens over MCP and their AI service is not visible here, so for
//! them GhostDesk counts what it can see: screens opened, actions, and screenshots sent or left out
//! (an image costs roughly [`IMAGE_TOKENS`] tokens, so a screenshot left out is that much saved).
//!
//! Counts are stored in `<data root>/usage.json`, one bucket per local day and source, for the last
//! [`KEEP_DAYS`] days. Windows uses UTC days.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Days of history kept.
pub const KEEP_DAYS: usize = 120;
/// Rough tokens of one 1280x800 screenshot (width x height / 750).
pub const IMAGE_TOKENS: u64 = 1_365;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Counts {
    #[serde(default)]
    pub in_tokens: u64,
    #[serde(default)]
    pub out_tokens: u64,
    /// Steps an agent inside GhostDesk took
    #[serde(default)]
    pub steps: u64,
    /// Actions outside agents sent to a screen
    #[serde(default)]
    pub actions: u64,
    /// Screens outside agents opened
    #[serde(default)]
    pub screens: u64,
    #[serde(default)]
    pub shots_sent: u64,
    #[serde(default)]
    pub shots_saved: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        self.in_tokens += o.in_tokens;
        self.out_tokens += o.out_tokens;
        self.steps += o.steps;
        self.actions += o.actions;
        self.screens += o.screens;
        self.shots_sent += o.shots_sent;
        self.shots_saved += o.shots_saved;
    }

    pub fn tokens(&self) -> u64 {
        self.in_tokens + self.out_tokens
    }

    /// Steps and actions together: what an agent did, wherever it ran.
    pub fn work(&self) -> u64 {
        self.steps + self.actions
    }

    /// Tokens not spent on screenshots that were left out.
    pub fn saved_tokens(&self) -> u64 {
        self.shots_saved * IMAGE_TOKENS
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Data {
    /// "YYYY-MM-DD" → source → counts
    #[serde(default)]
    pub days: BTreeMap<String, BTreeMap<String, Counts>>,
}

pub struct Usage {
    data: Mutex<Data>,
    path: Option<PathBuf>,
    dirty: Mutex<bool>,
}

/// The process-wide store (a file in the data folder; in tests only in memory).
pub fn global() -> &'static Usage {
    static U: OnceLock<Usage> = OnceLock::new();
    U.get_or_init(|| if cfg!(test) { Usage::in_memory() } else { Usage::load() })
}

/// "YYYY-MM-DD" of a unix time in local time (UTC on Windows).
pub fn day_of(unix: i64) -> String {
    #[cfg(unix)]
    {
        // SAFETY: localtime_r only writes into the struct we pass
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = unix as libc::time_t;
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
        }
    }
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Usage {
    pub fn in_memory() -> Self {
        Usage { data: Mutex::new(Data::default()), path: None, dirty: Mutex::new(false) }
    }

    fn load() -> Self {
        let path = crate::platform::paths::data_root().join("usage.json");
        let data = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Usage { data: Mutex::new(data), path: Some(path), dirty: Mutex::new(false) }
    }

    /// Adds to today's counts of `source`.
    pub fn record(&self, source: &str, f: impl FnOnce(&mut Counts)) {
        self.record_on(&day_of(now_unix()), source, f);
    }

    pub fn record_on(&self, day: &str, source: &str, f: impl FnOnce(&mut Counts)) {
        let mut d = self.data.lock();
        f(d.days.entry(day.to_string()).or_default().entry(source.to_string()).or_default());
        while d.days.len() > KEEP_DAYS {
            let oldest = d.days.keys().next().cloned().expect("not empty");
            d.days.remove(&oldest);
        }
        *self.dirty.lock() = true;
    }

    pub fn tokens(&self, source: &str, input: u64, output: u64) {
        self.record(source, |c| {
            c.in_tokens += input;
            c.out_tokens += output;
        });
    }

    pub fn step(&self, source: &str) {
        self.record(source, |c| c.steps += 1);
    }

    pub fn action(&self, source: &str) {
        self.record(source, |c| c.actions += 1);
    }

    pub fn screen(&self, source: &str) {
        self.record(source, |c| c.screens += 1);
    }

    pub fn shot(&self, source: &str, sent: bool) {
        self.record(source, |c| if sent { c.shots_sent += 1 } else { c.shots_saved += 1 });
    }

    /// A copy of the days from `from` ("YYYY-MM-DD", inclusive) on.
    pub fn since(&self, from: &str) -> BTreeMap<String, BTreeMap<String, Counts>> {
        self.data.lock().days.range(from.to_string()..).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Writes the file if something changed since the last write.
    pub fn save_if_dirty(&self) {
        let Some(path) = &self.path else { return };
        if !std::mem::take(&mut *self.dirty.lock()) {
            return;
        }
        let bytes = match serde_json::to_vec(&*self.data.lock()) {
            Ok(b) => b,
            Err(_) => return,
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            *self.dirty.lock() = true;
        }
    }
}

/// Compact number for the Usage tab: 950, 1.2k, 34k, 5.6M.
pub fn short_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1e3),
        10_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=9_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{}M", n / 1_000_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_147), (2025, 2, 28));
    }

    #[test]
    fn counts_add_up_per_day_and_source() {
        let u = Usage::in_memory();
        u.record_on("2026-11-18", "Claude · sonnet", |c| c.in_tokens += 100);
        u.record_on("2026-11-18", "Claude · sonnet", |c| c.out_tokens += 20);
        u.record_on("2026-11-19", "Codex", |c| c.actions += 3);
        let all = u.since("2026-11-01");
        assert_eq!(all.len(), 2);
        assert_eq!(all["2026-11-18"]["Claude · sonnet"].tokens(), 120);
        assert_eq!(u.since("2026-11-19").len(), 1);
        u.record_on("2026-11-19", "Codex", |c| c.shots_saved += 2);
        assert_eq!(u.since("2026-11-19")["2026-11-19"]["Codex"].saved_tokens(), 2 * IMAGE_TOKENS);
    }

    #[test]
    fn old_days_are_dropped() {
        let u = Usage::in_memory();
        for i in 0..(KEEP_DAYS + 5) {
            u.record_on(&format!("2026-{:02}-{:02}", 1 + i / 28, 1 + i % 28), "a", |c| c.steps += 1);
        }
        assert_eq!(u.since("0000").len(), KEEP_DAYS);
    }

    #[test]
    fn saved_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("gd-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("usage.json");
        let u = Usage { data: Mutex::new(Data::default()), path: Some(path.clone()), dirty: Mutex::new(false) };
        u.record_on("2026-11-19", "Codex", |c| c.screens += 1);
        u.save_if_dirty();
        let back: Data = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.days["2026-11-19"]["Codex"].screens, 1);
        // nothing changed: nothing written
        std::fs::remove_file(&path).unwrap();
        u.save_if_dirty();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_counts() {
        assert_eq!(short_count(950), "950");
        assert_eq!(short_count(1_234), "1.2k");
        assert_eq!(short_count(34_000), "34k");
        assert_eq!(short_count(5_600_000), "5.6M");
    }
}
