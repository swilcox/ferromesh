//! `ferromeshd rebuild`: recreate the database from the raw log.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ferromesh_store::Store;
use jiff::Timestamp;
use tracing::{info, warn};

use crate::config::Config;
use crate::pipeline::{self, Tally};
use crate::rawlog;

const CHUNK: usize = 1000;

/// SQLite keeps `-wal` and `-shm` files beside a database; they move with it.
const SIDECARS: [&str; 3] = ["", "-wal", "-shm"];

pub fn run(config: &Config, replace: bool) -> Result<()> {
    let raw_dir = config.raw_dir();
    ensure!(raw_dir.exists(), "no raw log at {}", raw_dir.display());
    let current_path = config.db_path();
    let rebuilt_path = config.data_dir.join("ferromesh.rebuild.db");
    remove_database(&rebuilt_path)?;

    let current = current_path.exists().then(|| Store::open(&current_path)).transpose()?;
    let mut rebuilt = Store::open(&rebuilt_path)?;
    // Channels aren't in the raw log: carry them over, then add new configured ones.
    if let Some(current) = &current {
        for channel in current.channels()? {
            rebuilt.add_channel(&channel.name, &channel.key, channel.kind, channel.added_at)?;
        }
    }
    pipeline::add_configured_channels(&mut rebuilt, config)?;

    let mut tally = Tally::default();
    let mut unreadable = 0u64;
    for path in rawlog::files(&raw_dir)? {
        let mut chunk = Vec::with_capacity(CHUNK);
        for record in rawlog::read(&path)? {
            match record {
                Ok(record) => chunk.push(record),
                Err(e) => {
                    unreadable += 1;
                    warn!("skipping: {e:#}");
                }
            }
            if chunk.len() == CHUNK {
                pipeline::ingest(&mut rebuilt, &chunk, &mut tally)?;
                chunk.clear();
            }
        }
        pipeline::ingest(&mut rebuilt, &chunk, &mut tally)?;
        info!(file = %path.display(), "replayed");
    }
    info!(%tally, unreadable, "rebuilt");
    println!("{}", pipeline::format_counts(&rebuilt.counts()?));

    let rebuilt_digest = rebuilt.digest()?;
    match &current {
        Some(current) => {
            let current_digest = current.digest()?;
            if current_digest == rebuilt_digest {
                info!(digest = %rebuilt_digest, "rebuilt database matches the current one");
            } else {
                warn!(
                    current = %current_digest,
                    rebuilt = %rebuilt_digest,
                    "rebuilt database differs from the current one"
                );
            }
        }
        None => info!(digest = %rebuilt_digest, "no current database to compare with"),
    }
    drop(current);
    drop(rebuilt);

    if !replace {
        info!(path = %rebuilt_path.display(), "left the rebuilt database alongside; --replace swaps it in");
        return Ok(());
    }
    if current_path.exists() {
        let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
        let backup = config.data_dir.join(format!("ferromesh.db.bak-{stamp}"));
        move_database(&current_path, &backup)?;
        info!(backup = %backup.display(), "kept the previous database");
    }
    move_database(&rebuilt_path, &current_path)?;
    info!(path = %current_path.display(), "replaced the database");
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_database(path: &Path) -> Result<()> {
    for suffix in SIDECARS {
        let file = sidecar(path, suffix);
        if file.exists() {
            fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
        }
    }
    Ok(())
}

fn move_database(from: &Path, to: &Path) -> Result<()> {
    for suffix in SIDECARS {
        let file = sidecar(from, suffix);
        if file.exists() {
            let target = sidecar(to, suffix);
            fs::rename(&file, &target)
                .with_context(|| format!("moving {} to {}", file.display(), target.display()))?;
        }
    }
    Ok(())
}
