//! The raw log: every MQTT message as received, one JSON line each.
//!
//! Live messages append to `raw/YYYY-MM-DD.jsonl` (UTC day), flushed and
//! synced after every batch. Finished days are compressed to `.jsonl.zst`, and
//! imports write their own `import-*.jsonl.zst`. The database can always be
//! rebuilt from these files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

const ZSTD_LEVEL: i32 = 19;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRecord {
    /// When ferromesh received it; imports use the observer's timestamp.
    pub received_at: Timestamp,
    /// `mqtt://host:port` or `import:<label>`.
    pub source: String,
    pub topic: String,
    pub payload: String,
}

pub struct RawLogWriter {
    dir: PathBuf,
    day: Option<(Date, BufWriter<File>)>,
}

impl RawLogWriter {
    /// Opens the log directory, compressing day files left from earlier days.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let today = utc_date(Timestamp::now());
        for (date, path) in day_files(&dir)? {
            if date < today {
                compress(&path)?;
            }
        }
        Ok(Self { dir, day: None })
    }

    pub fn append(&mut self, records: &[RawRecord]) -> Result<()> {
        for record in records {
            let date = utc_date(record.received_at);
            if self.day.as_ref().map(|(day, _)| *day) != Some(date) {
                self.roll_to(date)?;
            }
            let (_, out) = self.day.as_mut().expect("rolled to the record's day");
            serde_json::to_writer(&mut *out, record)?;
            out.write_all(b"\n")?;
        }
        if let Some((_, out)) = &mut self.day {
            out.flush()?;
            out.get_ref().sync_data()?;
        }
        Ok(())
    }

    fn roll_to(&mut self, date: Date) -> Result<()> {
        if let Some((previous, mut out)) = self.day.take() {
            out.flush()?;
            out.get_ref().sync_all()?;
            drop(out);
            // Only a finished day is compressed. If the clock steps backwards,
            // the current day stays open and is reopened when time catches up.
            if previous < date {
                compress(&self.dir.join(format!("{previous}.jsonl")))?;
            }
        }
        let path = self.dir.join(format!("{date}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        self.day = Some((date, BufWriter::new(file)));
        Ok(())
    }
}

/// Every raw file in replay order: days oldest first, then imports.
pub fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if name.ends_with(".jsonl") || name.ends_with(".jsonl.zst") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Reads one raw file. Lines that don't parse, such as one cut short by a
/// crash, come back as errors for the caller to skip.
pub fn read(path: &Path) -> Result<impl Iterator<Item = Result<RawRecord>> + use<>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|ext| ext == "zst") {
        Box::new(BufReader::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Box::new(BufReader::new(file))
    };
    let name = path.display().to_string();
    Ok(reader.lines().enumerate().map(move |(index, line)| {
        let line = line.with_context(|| format!("{name}:{}: unreadable", index + 1))?;
        serde_json::from_str(&line)
            .with_context(|| format!("{name}:{}: not a raw record", index + 1))
    }))
}

/// Writes an import's records to their own compressed file.
pub fn write_import(dir: &Path, label: &str, records: &[RawRecord]) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = format!("import-{}-{label}", Timestamp::now().strftime("%Y%m%dT%H%M%SZ"));
    let partial = dir.join(format!("{stem}.partial"));
    let path = dir.join(format!("{stem}.jsonl.zst"));

    let mut encoder = zstd::stream::write::Encoder::new(File::create(&partial)?, ZSTD_LEVEL)?;
    for record in records {
        serde_json::to_writer(&mut encoder, record)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?.sync_all()?;
    fs::rename(&partial, &path)?;
    Ok(path)
}

fn utc_date(at: Timestamp) -> Date {
    at.to_zoned(TimeZone::UTC).date()
}

fn day_files(dir: &Path) -> Result<Vec<(Date, PathBuf)>> {
    let mut days = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let date = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(|stem| stem.parse::<Date>().ok());
        if let Some(date) = date {
            days.push((date, path));
        }
    }
    Ok(days)
}

/// Compresses a finished day file. If that day was compressed before, the new
/// frame is appended: concatenated zstd frames still decode as one stream.
fn compress(path: &Path) -> Result<()> {
    let target = path.with_extension("jsonl.zst");
    let partial = path.with_extension("partial");
    zstd::stream::copy_encode(File::open(path)?, File::create(&partial)?, ZSTD_LEVEL)
        .with_context(|| format!("compressing {}", path.display()))?;
    if target.exists() {
        let mut out = OpenOptions::new().append(true).open(&target)?;
        io::copy(&mut File::open(&partial)?, &mut out)?;
        out.sync_all()?;
        fs::remove_file(&partial)?;
    } else {
        File::open(&partial)?.sync_all()?;
        fs::rename(&partial, &target)?;
    }
    fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(at: &str, n: u32) -> RawRecord {
        RawRecord {
            received_at: at.parse().unwrap(),
            source: "test".into(),
            topic: "meshcore/BNA/00/packets".into(),
            payload: format!(r#"{{"n":{n}}}"#),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        files(dir)
            .unwrap()
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn read_all(dir: &Path) -> Vec<RawRecord> {
        files(dir)
            .unwrap()
            .iter()
            .flat_map(|path| read(path).unwrap())
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn days_roll_over_and_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RawLogWriter::open(dir.path()).unwrap();
        let records = [
            record("2026-09-12T23:59:59Z", 1),
            record("2026-09-13T00:00:01Z", 2),
            record("2026-09-13T00:00:02Z", 3),
        ];
        log.append(&records[..1]).unwrap();
        log.append(&records[1..]).unwrap();

        assert_eq!(names(dir.path()), ["2026-09-12.jsonl.zst", "2026-09-13.jsonl"]);
        assert_eq!(read_all(dir.path()), records);
    }

    #[test]
    fn reopened_day_appends_a_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RawLogWriter::open(dir.path()).unwrap();
        let records = [
            record("2026-09-12T10:00:00Z", 1),
            record("2026-09-13T10:00:00Z", 2),
            record("2026-09-12T11:00:00Z", 3), // clock stepped back
            record("2026-09-13T11:00:00Z", 4),
        ];
        log.append(&records).unwrap();

        assert_eq!(names(dir.path()), ["2026-09-12.jsonl.zst", "2026-09-13.jsonl"]);
        let [one, two, three, four] = records;
        assert_eq!(read_all(dir.path()), [one, three, two, four]);
    }

    #[test]
    fn imports_replay_after_days() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RawLogWriter::open(dir.path()).unwrap();
        log.append(&[record("2026-09-12T10:00:00Z", 1)]).unwrap();
        let import = write_import(dir.path(), "old", &[record("2026-09-01T00:00:00Z", 2)]).unwrap();

        let names = names(dir.path());
        assert_eq!(names.len(), 2);
        assert_eq!(names[1], import.file_name().unwrap().to_string_lossy());
        assert_eq!(read_all(dir.path()).len(), 2);
    }
}
