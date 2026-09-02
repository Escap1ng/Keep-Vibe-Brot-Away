//! 加密存储:AES-256-GCM,密钥由 HKDF(机器指纹,随机 salt) 派生。禁止 MD5。
//!
//! secrets.enc 结构(JSON):{ salt, nonce, data } 其中 data = AES-GCM(keys_json)

use std::path::PathBuf;
use std::collections::HashMap;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

pub const PROVIDERS: &[&str] = &["openai", "google", "anthropic"];

/// 机器指纹:Windows 用 MachineGuid,macOS 用 IOPlatformUUID,Linux 用 /etc/machine-id
pub fn machine_id() -> String {
    #[cfg(target_os = "windows")]
    {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(val) = hklm.open_subkey("SOFTWARE\\Microsoft\\Cryptography") {
            if let Ok(guid) = val.get_value::<String, _>("MachineGuid") {
                return guid;
            }
        }
        // 兜底:基于环境+架构的指纹(质量较低但可用)
        let fallback = format!("{}-{:?}", std::env::var("COMPUTERNAME").unwrap_or_default(), std::env::consts::ARCH);
        return fallback;
    }
    #[cfg(target_os = "macos")]
    {
        // IOPlatformUUID(ioreg);失败回退 hostname
        if let Ok(out) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(pos) = s.find("IOPlatformUUID") {
                // 形如: IOPlatformUUID = "XXXXXXXX-XXXX-…"
                if let Some(v) = s[pos..].split('"').nth(1) {
                    return v.to_string();
                }
            }
        }
        std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "mac-unknown".into())
    }
    #[cfg(target_os = "linux")]
    {
        let id = std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .unwrap_or_else(|_| "linux-unknown".into());
        id.trim().to_string()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "unknown".into()
    }
}

/// HKDF-SHA256(machine_id, salt) → 32 字节 AES 密钥
pub fn derive_key(machine_id: &str, salt: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), machine_id.as_bytes());
    let mut key = [0u8; 32];
    hk.expand(b"kvba-v2-secrets", &mut key).expect("HKDF 不能失败");
    key
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct KeysFile {
    #[serde(default)] pub keys: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub struct SecretEnvelope {
    pub salt: String,
    pub nonce: String,
    pub data: String,
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()
        })
        .collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// 密钥环:负责加密读写 dataDir/secrets.enc
pub struct Keyring {
    path: PathBuf,
}

impl Keyring {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("secrets.enc"),
        }
    }

    /// 加密并写盘(生成新 salt/nonce)
    pub fn save(&self, keys: &KeysFile) -> anyhow::Result<()> {
        let machine = machine_id();
        let salt = random_bytes::<SALT_LEN>();
        let nonce = random_bytes::<NONCE_LEN>();
        let key = derive_key(&machine, &salt);

        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let plain = serde_json::to_vec(keys)?;
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("AES-GCM 加密失败"))?;

        let env = SecretEnvelope {
            salt: hex_encode(&salt),
            nonce: hex_encode(&nonce),
            data: hex_encode(&ct),
        };
        std::fs::write(&self.path, serde_json::to_string(&env)?)?;
        Ok(())
    }

    fn decrypt(&self) -> Option<KeysFile> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let env: SecretEnvelope = serde_json::from_str(&raw).ok()?;
        let salt = hex_decode(&env.salt)?;
        let nonce = hex_decode(&env.nonce)?;
        let data = hex_decode(&env.data)?;
        let key = derive_key(&machine_id(), &salt);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce), data.as_slice())
            .ok()?;
        serde_json::from_slice::<KeysFile>(&plain).ok()
    }

    pub fn load(&self) -> KeysFile {
        self.decrypt().unwrap_or_default()
    }

    pub fn get(&self, provider: &str) -> Option<String> {
        self.load()
            .keys
            .get(provider)
            .filter(|v| !v.is_empty())
            .cloned()
    }

    pub fn set(&self, provider: &str, key: &str) -> anyhow::Result<()> {
        let mut keys = self.load();
        keys.keys.insert(provider.to_string(), key.to_string());
        self.save(&keys)
    }

    pub fn has_any(&self) -> bool {
        self.load().keys.values().any(|v| !v.is_empty())
    }

    /// 返回"已配 key 的 provider" 列表(不泄漏密钥本体)
    pub fn present_providers(&self) -> Vec<String> {
        self.load()
            .keys
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// 设置面板保存密钥
#[tauri::command]
pub fn secrets_put(
    state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>,
    provider: String,
    key: String,
) -> Result<(), String> {
    let dir = state.data_dir.clone();
    let ring = Keyring::new(&dir);
    let key = key.trim().to_string();
    if key.is_empty() {
        // 清空
        let _ = ring.set(&provider, "");
    } else {
        ring.set(&provider, &key).map_err(|e| format!("写入失败:{e}"))?;
    }
    Ok(())
}

/// 测试是否已配置 provider(用于设置面板显示,不返回密钥)
#[tauri::command]
pub fn secrets_test(state: tauri::State<'_, std::sync::Arc<crate::state::AppState>>) -> Vec<String> {
    let ring = Keyring::new(&state.data_dir);
    ring.present_providers()
}
