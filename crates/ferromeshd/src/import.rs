//! `ferromeshd import`: load a capture from the Python mqtt_observer watcher.
//!
//! Each of its JSONL lines is an MQTT payload object with the topic merged
//! in. The topic is split back out, and records are dated by the observer's
//! own timestamp because the watcher didn't keep its receive time.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::config::Config;
use crate::pipeline::{self, Tally};
use crate::rawlog::{self, RawRecord};

const CHUNK: usize = 1000;

pub fn run(config: &Config, file: &Path, label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "label may only contain letters, digits, '-' and '_'"
    );
    let source = format!("import:{label}");
    let reader =
        BufReader::new(File::open(file).with_context(|| format!("opening {}", file.display()))?);

    let mut records = Vec::new();
    let mut skipped = 0;
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match convert(&line, &source) {
            Ok(record) => records.push(record),
            // The watcher may still be appending, leaving a partial last line.
            Err(e) => {
                skipped += 1;
                warn!(line = index + 1, "skipping: {e:#}");
            }
        }
    }

    // Raw log first, so a rebuild includes the import.
    let raw_file = rawlog::write_import(&config.raw_dir(), label, &records)?;
    info!(records = records.len(), skipped, file = %raw_file.display(), "wrote raw import");

    let mut store = pipeline::open_store(config)?;
    let mut tally = Tally::default();
    for chunk in records.chunks(CHUNK) {
        pipeline::ingest(&mut store, chunk, &mut tally)?;
    }
    info!(%tally, "imported");
    println!("{}", pipeline::format_counts(&store.counts()?));
    Ok(())
}

fn convert(line: &str, source: &str) -> Result<RawRecord> {
    let mut fields: Map<String, Value> = serde_json::from_str(line).context("not a JSON object")?;
    let Some(Value::String(topic)) = fields.remove("topic") else {
        bail!("no topic");
    };
    let received_at = fields
        .get("timestamp")
        .and_then(Value::as_str)
        .context("no timestamp")?
        .parse()
        .context("bad timestamp")?;
    Ok(RawRecord {
        received_at,
        source: source.to_owned(),
        topic,
        payload: serde_json::to_string(&fields)?,
    })
}
