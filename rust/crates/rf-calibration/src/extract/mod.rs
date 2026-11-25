//! Build the measured-link dataset from capture snapshots.

mod corescope;
mod corescope_hops;
mod meshtastic;
mod model;
mod privacy;
mod quality;
mod stats;
mod summary;
#[cfg(test)]
mod tests;

use std::{
    collections::BTreeSet,
    fs,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use repeaternet_providers::sources::ProviderRegistry;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::dataset::Evidence;

impl Evidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::CorescopeLastHop => "corescope_last_hop",
            Self::CorescopeZeroHop => "corescope_zero_hop",
            Self::CorescopeNonDetection => "corescope_non_detection",
            Self::CorescopeRelayHop => "corescope_relay_hop",
            Self::CorescopeTraceHop => "corescope_trace_hop",
            Self::CorescopeNeighborReport => "corescope_neighbor_report",
            Self::MeshtasticGatewayRx => "meshtastic_gateway_rx",
            Self::MeshtasticTraceroute => "meshtastic_traceroute",
            Self::MeshtasticNeighbor => "meshtastic_neighbor",
        }
    }
}

/// Extract the measured-link dataset from `<work>/snapshots` into
/// `<work>/dataset`.
pub fn run(snapshots: &Path, dataset: &Path) -> Result<()> {
    let resolved = snapshots
        .canonicalize()
        .context("resolve calibration snapshot directory")?;
    if let Ok(root) = crate::paths::repeaternet_data_root()
        && let Ok(root) = root.canonicalize()
    {
        ensure!(
            resolved != root && !resolved.starts_with(root.join("data")),
            "select a separate snapshot directory outside the original captures and planner data"
        );
    }
    let entries = fs::read_dir(resolved).context("read calibration snapshot directory")?;
    let work = snapshots
        .parent()
        .context("snapshot directory has no parent")?;
    let salt = privacy::load_salt(work)?;
    let mut sources = Vec::new();
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut notes = vec![corescope::NEIGHBOR_NOTE];
    let mut source_ids = BTreeSet::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".sqlite3") {
            continue;
        }
        let read = if name.starts_with("corescope-capture") {
            corescope::read
        } else if name.starts_with("meshtastic-capture-") {
            meshtastic::read
        } else {
            notes.push(if name == "sondehub-import.sqlite3" {
                "SondeHub snapshot omitted from this terrestrial measured-link dataset."
            } else {
                "An unrecognized SQLite snapshot was omitted."
            });
            continue;
        };
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open snapshot {name} read-only"))?;
        let source = read(&db, &salt).with_context(|| format!("extract snapshot {name}"))?;
        ensure!(
            source_ids.insert(source.id.clone()),
            "multiple snapshots for capture source {}",
            source.id
        );
        sources.push(source);
    }
    ensure!(
        !sources.is_empty(),
        "no CoreScope or Meshtastic snapshots found"
    );
    sources.sort_by(|a, b| a.id.cmp(&b.id));

    let registry = ProviderRegistry::builtin();
    let provider = registry
        .terrain()
        .iter()
        .find(|provider| provider.id == "kartverket-nhm")
        .context("built-in Kartverket NHM terrain provider is missing")?;
    let quality = quality::PositionQuality::from_sources(&sources);
    let mut links = Vec::new();
    let mut nodes = Vec::new();
    for source in &sources {
        let (source_links, source_nodes) = source.records(&provider.coverage, &quality);
        links.extend(source_links);
        nodes.extend(source_nodes);
    }
    links.sort_by(|a, b| a.link_id.cmp(&b.link_id));
    nodes.sort_by(|a, b| (&a.source, &a.node).cmp(&(&b.source, &b.node)));
    let summary = summary::Summary::new(&sources, &links, &nodes, notes);

    fs::create_dir_all(dataset)?;
    let links_file = stage(dataset, |file| write_json_lines(file, &links))?;
    let nodes_file = stage(dataset, |file| write_json_lines(file, &nodes))?;
    let summary_file = stage(dataset, |file| {
        serde_json::to_writer_pretty(&mut *file, &summary)?;
        writeln!(file)?;
        Ok(())
    })?;
    for (file, name) in [
        (links_file, "links.jsonl"),
        (nodes_file, "nodes.jsonl"),
        (summary_file, "summary.json"),
    ] {
        file.persist(dataset.join(name))
            .with_context(|| format!("publish {name}"))?;
    }
    summary.print();
    Ok(())
}

fn stage(
    dataset: &Path,
    write: impl FnOnce(&mut BufWriter<&mut fs::File>) -> Result<()>,
) -> Result<NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .suffix(".partial")
        .tempfile_in(dataset)?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        write(&mut writer)?;
        writer.flush()?;
    }
    file.as_file().sync_all()?;
    Ok(file)
}

fn write_json_lines(mut file: impl Write, records: &[impl Serialize]) -> Result<()> {
    for record in records {
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
    }
    Ok(())
}
