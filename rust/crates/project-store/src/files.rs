use std::{
    fs::{self, File, FileTimes, Metadata},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{Error, Result};

pub(crate) const TERRAIN_DIRECTORIES: [&str; 4] = [
    "dtm",
    "dom",
    "terrain-generations",
    "coverage-terrain-generations",
];

pub(crate) fn directory_parts(directory: &str) -> Result<Vec<&str>> {
    if directory == "." {
        return Ok(Vec::new());
    }
    let components: Vec<_> = directory.split(['/', '\\']).collect();
    if components.iter().copied().all(valid_component) {
        Ok(components)
    } else {
        Err(Error::UnsafeStoragePath)
    }
}

fn valid_component(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c))
    {
        return false;
    }
    let device = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let device = device.to_ascii_uppercase();
    if matches!(
        device.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        matches!(
            device.strip_prefix(prefix),
            Some("0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        )
    })
}

pub(crate) fn checked_directory(
    base: &Path,
    components: impl IntoIterator<Item = impl AsRef<Path>>,
    create: bool,
) -> Result<PathBuf> {
    let mut path = base.to_path_buf();
    for component in components {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                require_directory(&fs::symlink_metadata(&path)?)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(metadata) => require_directory(&metadata)?,
        }
    }
    Ok(path)
}

pub(crate) fn require_directory(metadata: &Metadata) -> Result<()> {
    if metadata.is_dir() && !is_link(metadata) {
        Ok(())
    } else {
        Err(Error::UnsafeStoragePath)
    }
}

pub(crate) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

pub(crate) fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn write_new_plan(path: &Path, plan: &Value) -> Result<()> {
    let mut file = File::create_new(path)?;
    serde_json::to_writer_pretty(&mut file, plan)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn copy_terrain(source: &Path, target: &Path) -> Result<()> {
    for name in TERRAIN_DIRECTORIES {
        let source = source.join(name);
        match fs::symlink_metadata(&source) {
            Ok(metadata) => copy_tree(&source, &target.join(name), &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path, metadata: &Metadata) -> Result<()> {
    require_directory(metadata)?;
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let target = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if is_link(&metadata) {
            return Err(Error::UnsafeStoragePath);
        }
        if metadata.is_dir() {
            copy_tree(&source, &target, &metadata)?;
        } else if metadata.is_file() {
            io::copy(&mut File::open(&source)?, &mut File::create_new(&target)?)?;
            copy_metadata(&target, &metadata)?;
        } else {
            return Err(Error::UnsafeStoragePath);
        }
    }
    copy_metadata(target, metadata)
}

fn copy_metadata(target: &Path, source: &Metadata) -> Result<()> {
    fs::set_times(
        target,
        FileTimes::new()
            .set_accessed(source.accessed()?)
            .set_modified(source.modified()?),
    )?;
    fs::set_permissions(target, source.permissions())?;
    Ok(())
}

pub(crate) fn storage_usage(path: &Path) -> Result<u64> {
    let mut bytes = 0;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            bytes += storage_usage(&entry.path())?;
        } else if metadata.is_file() {
            bytes += metadata.len();
        }
    }
    Ok(bytes)
}

// Validate the entire tree before a permanent deletion, including reparse
// points whose semantics differ between Windows filesystems.
pub(crate) fn check_delete_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link(&metadata) {
            return Err(Error::UnsafeStoragePath);
        }
        if metadata.is_dir() {
            check_delete_tree(&entry.path())?;
        }
    }
    Ok(())
}
