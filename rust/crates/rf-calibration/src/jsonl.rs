use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;

pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) => {
                let line = line.strip_prefix('\u{feff}').unwrap_or(&line).trim();
                (!line.is_empty()).then(|| {
                    serde_json::from_str(line)
                        .with_context(|| format!("{} line {}", path.display(), index + 1))
                })
            }
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

pub fn read_if_present<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    match read(path) {
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(Vec::new())
        }
        result => result,
    }
}

pub fn write<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let mut writer = AtomicWriter::new(path)?;
    for row in rows {
        writer.push(row)?;
    }
    writer.publish(path)
}

pub struct AtomicWriter(BufWriter<NamedTempFile>);

impl AtomicWriter {
    pub fn new(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        Ok(Self(BufWriter::new(NamedTempFile::new_in(parent)?)))
    }

    pub fn push(&mut self, row: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.0, row)?;
        self.0.write_all(b"\n")?;
        Ok(())
    }

    pub fn publish(mut self, path: &Path) -> Result<()> {
        self.0.flush()?;
        let staged = self.0.into_inner().map_err(|error| error.into_error())?;
        staged.as_file().sync_all()?;
        staged
            .persist(path)
            .with_context(|| format!("publish {}", path.display()))?;
        Ok(())
    }
}
