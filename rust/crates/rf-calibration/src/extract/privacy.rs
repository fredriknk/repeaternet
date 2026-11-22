use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

pub(super) fn load_salt(work: &Path) -> Result<String> {
    let path = work.join("salt.txt");
    let salt = match fs::read_to_string(&path) {
        Ok(salt) => salt,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut random = [0; 32];
            getrandom::fill(&mut random)?;
            let mut stage = tempfile::Builder::new()
                .suffix(".partial")
                .tempfile_in(work)?;
            writeln!(stage, "{}", hex(&random))?;
            stage.as_file().sync_all()?;
            match stage.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error.into()),
            }
            fs::read_to_string(path).context("read calibration salt")?
        }
        Err(error) => return Err(error).context("read calibration salt"),
    };
    ensure!(!salt.trim().is_empty(), "calibration salt is empty");
    Ok(salt.trim().to_owned())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("write hex to string");
    }
    output
}

pub(super) fn node_id(salt: &str, namespace: &str, raw_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(salt);
    digest.update(namespace);
    digest.update(":");
    digest.update(raw_id.to_ascii_lowercase());
    hex(&digest.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_normalizes_keys_and_separates_protocols() {
        let key = node_id("salt", "corescope", "ABC");
        assert_eq!(key, node_id("salt", "corescope", "abc"));
        assert_ne!(key, node_id("salt", "meshtastic", "abc"));
        assert_ne!(key, node_id("another salt", "corescope", "abc"));
        assert_eq!(key, "f302bac9deecb878");
    }

    #[test]
    fn salt_is_created_once_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let salt = load_salt(dir.path()).unwrap();
        assert_eq!(salt.len(), 64);
        assert!(salt.bytes().all(|byte| byte.is_ascii_hexdigit()));
        fs::write(dir.path().join("salt.txt"), format!("  {salt}\r\n")).unwrap();
        assert_eq!(load_salt(dir.path()).unwrap(), salt);
    }
}
