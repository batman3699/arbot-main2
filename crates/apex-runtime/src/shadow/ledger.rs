//! The run's measurement ledgers (Task 8.5 R25): JSON lines, a file per UTC
//! day, so a fourteen-day run is fourteen files the analyzer globs.

use std::path::{Path, PathBuf};

/// `var/apex/race.jsonl` at 2026-10-12 is `var/apex/race-2026-10-12.jsonl`.
pub fn daily_path(base: &Path, at_ns: u64) -> PathBuf {
    let days = (at_ns / 1_000_000_000 / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("ledger");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    base.with_file_name(format!("{stem}-{y:04}-{m:02}-{d:02}.{ext}"))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard
/// Hinnant's `civil_from_days`).
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

/// Append one JSON line to the day's file, creating it as needed. A failed
/// write is returned to the caller, which counts it and carries on.
pub fn append_line(base: &Path, at_ns: u64, line: &impl serde::Serialize) -> std::io::Result<()> {
    use std::io::Write;
    let path = daily_path(base, at_ns);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    f.write_all(text.as_bytes())
}
