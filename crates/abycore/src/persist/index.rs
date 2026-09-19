//! Disposable discovery metadata. The JSONL log remains authoritative.
use super::{Discovery, fs};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
    time::SystemTime,
};

const NAME: &str = "summary.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u32,
    len: u64,
    modified: SystemTime,
    discovery: Discovery,
}

pub(super) fn read(dir: &Path, metadata: &fs::Metadata) -> Option<Discovery> {
    let file = fs::File::open(dir.join(NAME)).ok()?;
    // Titles are host input, but a corrupt index must not allocate unbounded memory.
    let index: Index = serde_json::from_reader(file.take(1024 * 1024)).ok()?;
    (index.version == 1
        && index.len == metadata.len()
        && index.modified == metadata.modified().ok()?)
    .then_some(index.discovery)
}

pub(super) fn write(
    dir: &Path,
    metadata: &fs::Metadata,
    discovery: &Discovery,
) -> std::io::Result<()> {
    let index = Index {
        version: 1,
        len: metadata.len(),
        modified: metadata.modified()?,
        discovery: discovery.clone(),
    };
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, &index)?;
    file.flush()?;
    // The cache can be lost on a crash; log durability never depends on it.
    file.persist(dir.join(NAME)).map_err(|error| error.error)?;
    Ok(())
}
