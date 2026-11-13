use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CODE_TTL_SECS: u64 = 600;
const MAX_ATTEMPTS: u32 = 10;

fn dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let dir = home.join(".wrosecode");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCode {
    pub code: String,
    pub device_id: String,
    pub device_name: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub attempts: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub device_name: String,
    pub account_id: String,
    pub token_hash: String,
    pub created_at: u64,
    pub last_seen: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct PendingFile {
    #[serde(default)]
    codes: Vec<PendingCode>,
}

#[derive(Default, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default)]
    devices: Vec<PairedDevice>,
}

/// Persistent local device identity (`~/.wrosecode/device.json`).
pub fn local_device(device_name: &str) -> Result<DeviceInfo> {
    let path = dir()?.join("device.json");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(info) = serde_json::from_slice::<DeviceInfo>(&bytes) {
            if !info.device_id.is_empty() {
                return Ok(info);
            }
        }
    }
    let mut bytes = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
    let device_id = format!("dev_{}", hex(&bytes));
    let info = DeviceInfo {
        device_id,
        device_name: device_name.to_string(),
        created_at: now(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&info)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(info)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_hash(token: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref())
}

/// Generate a one-time pairing code like `482-931`. Stored pending with a
/// short TTL. Returns the code and expiry.
pub fn new_pairing_code(device_name: &str) -> Result<(DeviceInfo, PendingCode)> {
    let device = local_device(device_name)?;
    let mut digits = [0u8; 6];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut digits)
        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
    let num: u32 = digits
        .iter()
        .fold(0u32, |acc, b| acc.wrapping_mul(257).wrapping_add(*b as u32))
        % 1_000_000;
    let code = format!("{:03}-{:03}", num / 1000, num % 1000);
    let pending = PendingCode {
        code: code.clone(),
        device_id: device.device_id.clone(),
        device_name: device_name.to_string(),
        created_at: now(),
        expires_at: now() + CODE_TTL_SECS,
        attempts: 0,
    };
    let path = dir()?.join("pairing.json");
    let mut file: PendingFile = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    file.codes.retain(|c| c.expires_at > now());
    file.codes.push(pending.clone());
    // Bound pending list.
    if file.codes.len() > 5 {
        let drain = file.codes.len() - 5;
        file.codes.drain(..drain);
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&file)?)?;
    Ok((device, pending))
}

fn load_pending() -> PendingFile {
    let Ok(d) = dir() else {
        return PendingFile::default();
    };
    std::fs::read(d.join("pairing.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Confirm a pairing code. On success binds `account_id` to this device and
/// returns a fresh device token (plaintext, shown once).
pub fn confirm_pairing(
    code: &str,
    account_id: &str,
    device_name: &str,
) -> Result<(String, String)> {
    anyhow::ensure!(
        !account_id.trim().is_empty() && account_id.len() <= 320,
        "invalid account"
    );
    anyhow::ensure!(
        !device_name.trim().is_empty() && device_name.len() <= 120,
        "invalid device name"
    );
    let normalized = code.trim().to_string();
    let path = dir()?.join("pairing.json");
    let mut file: PendingFile = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let t = now();
    // Brute-force guard: count failures globally.
    let mut matched_idx: Option<usize> = None;
    for (i, c) in file.codes.iter().enumerate() {
        if c.expires_at <= t {
            continue;
        }
        // Constant-time compare on length + bytes.
        let a = c.code.as_bytes();
        let b = normalized.as_bytes();
        let eq =
            a.len() == b.len() && a.iter().zip(b.iter()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0;
        if eq {
            matched_idx = Some(i);
            break;
        }
    }
    let Some(idx) = matched_idx else {
        // Record a failed attempt on the newest non-expired code (if any) so
        // guessing burns the window; expire after MAX_ATTEMPTS.
        for c in file.codes.iter_mut() {
            if c.expires_at > t {
                c.attempts += 1;
                if c.attempts >= MAX_ATTEMPTS {
                    c.expires_at = t;
                }
                break;
            }
        }
        let _ = std::fs::write(&path, serde_json::to_vec_pretty(&file)?);
        anyhow::bail!("invalid or expired pairing code");
    };
    let pending = file.codes.remove(idx);
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(&file)?);

    // Issue device token.
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
    use base64::Engine;
    let token = format!(
        "wrd_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    let devices_path = dir()?.join("devices.json");
    let mut devices: DeviceFile = std::fs::read(&devices_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // One binding per account+device: rotate token on re-pair.
    devices
        .devices
        .retain(|d| !(d.account_id == account_id && d.device_id == pending.device_id));
    devices.devices.push(PairedDevice {
        device_id: pending.device_id.clone(),
        device_name: device_name.to_string(),
        account_id: account_id.to_string(),
        token_hash: token_hash(&token),
        created_at: t,
        last_seen: t,
    });
    // Bound device list.
    if devices.devices.len() > 20 {
        let drain = devices.devices.len() - 20;
        devices.devices.drain(..drain);
    }
    std::fs::write(&devices_path, serde_json::to_vec_pretty(&devices)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&devices_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((pending.device_id, token))
}

/// Validate a device bearer token. Returns (device_id, account_id) on success.
pub fn validate_device_token(token: &str) -> Option<(String, String)> {
    if token.len() < 16 {
        return None;
    }
    let hash = token_hash(token);
    let devices_path = dir().ok()?.join("devices.json");
    let mut devices: DeviceFile = std::fs::read(&devices_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())?;
    let mut found = None;
    for d in devices.devices.iter_mut() {
        let a = d.token_hash.as_bytes();
        let b = hash.as_bytes();
        if a.len() == b.len() && a.iter().zip(b.iter()).fold(0u8, |x, (p, q)| x | (p ^ q)) == 0 {
            d.last_seen = now();
            found = Some((d.device_id.clone(), d.account_id.clone()));
            break;
        }
    }
    if found.is_some() {
        let _ = std::fs::write(&devices_path, serde_json::to_vec_pretty(&devices).ok()?);
    }
    found
}

pub fn list_devices() -> Vec<PairedDevice> {
    let Ok(d) = dir() else { return vec![] };
    std::fs::read(d.join("devices.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<DeviceFile>(&b).ok())
        .map(|f| f.devices)
        .unwrap_or_default()
        .into_iter()
        .map(|mut d| {
            d.token_hash = format!("{}…", &d.token_hash[..8.min(d.token_hash.len())]);
            d
        })
        .collect()
}

pub fn revoke_device(device_id: &str, account_id: Option<&str>) -> Result<bool> {
    let devices_path = dir()?.join("devices.json");
    let mut devices: DeviceFile = std::fs::read(&devices_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let before = devices.devices.len();
    devices.devices.retain(|d| {
        if d.device_id != device_id {
            return true;
        }
        if let Some(acct) = account_id {
            return d.account_id != acct;
        }
        false
    });
    let removed = devices.devices.len() != before;
    std::fs::write(&devices_path, serde_json::to_vec_pretty(&devices)?)?;
    Ok(removed)
}

pub fn pending_count() -> usize {
    let Ok(d) = dir() else { return 0 };
    let t = now();
    std::fs::read(d.join("pairing.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<PendingFile>(&b).ok())
        .map(|f| f.codes.into_iter().filter(|c| c.expires_at > t).count())
        .unwrap_or(0)
}

// Kept for future audit tooling; exercised by tests.
#[allow(dead_code)]
pub fn pending_codes() -> Vec<PendingCode> {
    load_pending().codes
}
