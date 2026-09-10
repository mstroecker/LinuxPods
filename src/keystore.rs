//! Persistent storage for AirPods BLE encryption keys.
//!
//! Stored at ~/.local/share/linuxpods/keys.json (XDG Base Directory), mode 0600:
//! {"version": 1, "keys": {"MAC": "base64-key", ...}}

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "linuxpods";
const KEYS_FILE: &str = "keys.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct KeyData {
    version: u32,
    /// MAC address -> base64-encoded key
    keys: HashMap<String, String>,
}

/// In-memory cache plus explicit load/save, mirroring the Go API.
pub struct Keystore {
    data_dir: PathBuf,
    keys: HashMap<String, Vec<u8>>,
}

impl Keystore {
    /// Creates the data directory (0700) if absent.
    pub fn new() -> Result<Self> {
        let data_dir = data_dir()?;
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;
        restrict_permissions(&data_dir, 0o700)?;
        Ok(Self {
            data_dir,
            keys: HashMap::new(),
        })
    }

    /// Test hook: use an explicit directory instead of the XDG one.
    pub fn with_dir(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        Ok(Self {
            data_dir,
            keys: HashMap::new(),
        })
    }

    fn path(&self) -> PathBuf {
        self.data_dir.join(KEYS_FILE)
    }

    /// Reads all keys from disk. A missing file is not an error.
    pub fn load(&mut self) -> Result<HashMap<String, Vec<u8>>> {
        let path = self.path();
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let raw = std::fs::read(&path)
            .with_context(|| format!("failed to read keys file {}", path.display()))?;
        let parsed: KeyData = serde_json::from_slice(&raw).context("failed to parse keys JSON")?;

        let mut keys = HashMap::with_capacity(parsed.keys.len());
        for (mac, b64) in parsed.keys {
            let key = B64
                .decode(b64.as_bytes())
                .with_context(|| format!("failed to decode key for {mac}"))?;
            keys.insert(mac, key);
        }

        self.keys = keys.clone();
        Ok(keys)
    }

    /// Writes all cached keys to disk with 0600 permissions.
    pub fn save(&self) -> Result<()> {
        let data = KeyData {
            version: FORMAT_VERSION,
            keys: self
                .keys
                .iter()
                .map(|(mac, key)| (mac.clone(), B64.encode(key)))
                .collect(),
        };

        let json = serde_json::to_vec_pretty(&data).context("failed to marshal keys to JSON")?;
        let path = self.path();
        std::fs::write(&path, json)
            .with_context(|| format!("failed to write keys file {}", path.display()))?;
        restrict_permissions(&path, 0o600)?;
        Ok(())
    }

    pub fn get(&self, mac: &str) -> Option<Vec<u8>> {
        self.keys.get(mac).cloned()
    }

    /// Rejects empty keys, as the Go version did.
    pub fn set(&mut self, mac: &str, key: &[u8]) -> Result<()> {
        anyhow::ensure!(!key.is_empty(), "encryption key cannot be empty");
        self.keys.insert(mac.to_string(), key.to_vec());
        Ok(())
    }

    pub fn remove(&mut self, mac: &str) {
        self.keys.remove(mac);
    }

    pub fn list(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.keys.clear();
    }

    pub fn all(&self) -> HashMap<String, Vec<u8>> {
        self.keys.clone()
    }
}

/// $XDG_DATA_HOME/linuxpods, falling back to ~/.local/share/linuxpods.
fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("failed to determine XDG data directory")?;
    Ok(base.join(APP_NAME))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Keystore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Keystore::with_dir(dir.path().to_path_buf()).unwrap(), dir)
    }

    #[test]
    fn missing_file_loads_empty() {
        let (mut ks, _d) = store();
        assert!(ks.load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let (mut ks, dir) = store();
        let key = vec![1u8; 16];
        ks.set("AA:BB:CC:DD:EE:FF", &key).unwrap();
        ks.save().unwrap();

        let mut reopened = Keystore::with_dir(dir.path().to_path_buf()).unwrap();
        let loaded = reopened.load().unwrap();
        assert_eq!(loaded.get("AA:BB:CC:DD:EE:FF"), Some(&key));
    }

    #[test]
    fn rejects_empty_key() {
        let (mut ks, _d) = store();
        assert!(ks.set("AA:BB:CC:DD:EE:FF", &[]).is_err());
    }

    #[test]
    fn reads_go_written_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(KEYS_FILE),
            r#"{"version":1,"keys":{"AA:BB:CC:DD:EE:FF":"AQIDBAUGBwgJCgsMDQ4PEA=="}}"#,
        )
        .unwrap();

        let mut ks = Keystore::with_dir(dir.path().to_path_buf()).unwrap();
        let loaded = ks.load().unwrap();
        assert_eq!(loaded["AA:BB:CC:DD:EE:FF"], (1u8..=16).collect::<Vec<u8>>());
    }

    #[test]
    fn save_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (mut ks, dir) = store();
        ks.set("AA:BB:CC:DD:EE:FF", &[9u8; 16]).unwrap();
        ks.save().unwrap();
        let mode = std::fs::metadata(dir.path().join(KEYS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
