//! Bitcoin Core JSON-RPC client.
//!
//! Uses cookie-file authentication by default (the `.cookie` file that
//! `bitcoind` writes on every startup).  Falls back to `rpcuser`/`rpcpassword`
//! from `bitcoin.conf` when no cookie is found.

use std::time::Duration;
use std::{
    fs::OpenOptions,
    io::{Read as _, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::Path,
};

use anyhow::{bail, Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BITCOIN_CONF_BYTES: u64 = 1024 * 1024;
const MAX_COOKIE_BYTES: u64 = 4096;

/// Lazily-built HTTP client (one per poll cycle is fine; keep it cheap).
fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build reqwest client")
}

// ── RPC types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    id: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<Value>,
}

/// Parsed result of `getblockchaininfo`.
#[derive(Debug, Clone, Default)]
pub struct BlockchainInfo {
    pub blocks: u64,
    pub headers: u64,
    pub verification_progress: f64,
}

// ── Authentication ────────────────────────────────────────────────────────────

/// Authentication credentials for Bitcoin RPC.
#[derive(Debug, Clone)]
pub struct RpcAuth {
    pub user: String,
    pub password: String,
    pub port: u16,
}

impl RpcAuth {
    /// Resolve credentials from the data directory.
    ///
    /// Preference order:
    ///   1. `.cookie` in the data dir root
    ///   2. `.cookie` in `<datadir>/mainnet/`
    ///   3. `rpcuser` / `rpcpassword` from `bitcoin.conf`
    ///   4. Hardcoded fallback ("bitcoin" / "bitcoinrpc")
    pub fn from_data_dir(data_dir: &Path) -> Self {
        let conf = BitcoinConf::load(data_dir);

        if let Some((user, password)) = conf.cookie_credentials {
            return Self {
                user,
                password,
                port: conf.port.unwrap_or(8332),
            };
        }

        Self {
            user: conf.rpcuser.unwrap_or_else(|| "bitcoin".to_owned()),
            password: conf.rpcpassword.unwrap_or_else(|| "bitcoinrpc".to_owned()),
            port: conf.port.unwrap_or(8332),
        }
    }
}

struct BitcoinConf {
    port: Option<u16>,
    rpcuser: Option<String>,
    rpcpassword: Option<String>,
    cookie_credentials: Option<(String, String)>,
}

impl BitcoinConf {
    fn load(data_dir: &Path) -> Self {
        let mut conf = Self {
            port: None,
            rpcuser: None,
            rpcpassword: None,
            cookie_credentials: None,
        };

        let Some(text) =
            read_bounded_regular(&data_dir.join("bitcoin.conf"), MAX_BITCOIN_CONF_BYTES)
        else {
            return conf;
        };

        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("rpcport=") {
                conf.port = rest.trim().parse().ok();
            }
            if let Some(v) = line.strip_prefix("rpcuser=") {
                conf.rpcuser = Some(v.trim().to_owned());
            }
            if let Some(v) = line.strip_prefix("rpcpassword=") {
                conf.rpcpassword = Some(v.trim().to_owned());
            }
        }

        for cookie_path in [
            data_dir.join(".cookie"),
            data_dir.join("mainnet").join(".cookie"),
        ] {
            if let Some(contents) = read_bounded_regular(&cookie_path, MAX_COOKIE_BYTES) {
                let contents = contents.trim();
                if let Some((user, password)) = contents.split_once(':') {
                    conf.cookie_credentials = Some((user.to_owned(), password.to_owned()));
                    break;
                }
            }
        }

        conf
    }
}

fn read_bounded_regular(path: &Path, limit: u64) -> Option<String> {
    let initial = std::fs::symlink_metadata(path).ok()?;
    if initial.file_type().is_symlink() || !initial.is_file() || initial.len() > limit {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > limit {
        return None;
    }
    String::from_utf8(bytes).ok()
}

// ── RPC call ─────────────────────────────────────────────────────────────────

/// Make a single synchronous-style async RPC call.
pub async fn call(auth: &RpcAuth, method: &str, params: Value) -> Result<Value> {
    let client = http_client()?;
    let url = format!("http://127.0.0.1:{}/", auth.port);

    let req = RpcRequest {
        jsonrpc: "1.0",
        id: "bnm",
        method,
        params,
    };

    let resp = client
        .post(&url)
        .basic_auth(&auth.user, Some(&auth.password))
        .json(&req)
        .send()
        .await
        .context("RPC HTTP request")?;

    let status = resp.status();
    if status == 401 {
        bail!("RPC authentication failed (401). Check bitcoin.conf credentials or .cookie file.");
    }

    let rpc_resp: RpcResponse = resp.json().await.context("parse RPC response")?;

    if let Some(err) = rpc_resp.error {
        bail!("RPC error: {err}");
    }

    rpc_resp.result.context("RPC result was null")
}

/// Call `getblockchaininfo` and return parsed data.
pub async fn get_blockchain_info(auth: &RpcAuth) -> Result<BlockchainInfo> {
    let v = call(auth, "getblockchaininfo", Value::Array(vec![])).await?;

    Ok(BlockchainInfo {
        blocks: v["blocks"].as_u64().unwrap_or(0),
        headers: v["headers"].as_u64().unwrap_or(0),
        verification_progress: v["verificationprogress"].as_f64().unwrap_or(0.0),
    })
}

/// Send the `stop` RPC command.
pub async fn stop_bitcoind(auth: &RpcAuth) -> Result<()> {
    call(auth, "stop", Value::Array(vec![])).await?;
    Ok(())
}

// ── Default bitcoin.conf generator ───────────────────────────────────────────

/// Create a minimal `bitcoin.conf` if one doesn't exist yet.
pub fn ensure_bitcoin_conf(data_dir: &Path) -> Result<()> {
    let data_dir =
        crate::platform::prepare_real_directory(data_dir, "Bitcoin data directory", true)?;
    let conf_path = data_dir.join("bitcoin.conf");
    match std::fs::symlink_metadata(&conf_path) {
        Ok(metadata) if metadata.file_type().is_file() => return Ok(()),
        Ok(_) => {
            bail!(
                "bitcoin.conf must be a regular file: {}",
                conf_path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect bitcoin.conf {}", conf_path.display()));
        }
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&conf_path)
        .with_context(|| format!("create bitcoin.conf {}", conf_path.display()))?;
    let contents = "# Bitcoin Core — auto-generated by BitEngine\n\
         server=1\n\
         txindex=1\n\
         rpcport=8332\n\
         rpcallowip=127.0.0.1\n\
         # Cookie-based authentication is active by default.\n";
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write bitcoin.conf {}", conf_path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync bitcoin.conf {}", conf_path.display()))?;
    std::fs::File::open(&data_dir)
        .with_context(|| format!("open Bitcoin data directory {}", data_dir.display()))?
        .sync_all()
        .with_context(|| format!("sync Bitcoin data directory {}", data_dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bitcoin_conf_creation_is_private_and_rejects_symlinks() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temporary = tempfile::tempdir()?;
        let data = temporary.path().join("BitcoinChain");
        ensure_bitcoin_conf(&data)?;
        assert_eq!(
            std::fs::metadata(data.join("bitcoin.conf"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&data)?.permissions().mode() & 0o777,
            0o700
        );

        let unrelated = temporary.path().join("unrelated.conf");
        let aliased_data = temporary.path().join("AliasedBitcoinChain");
        std::fs::write(&unrelated, b"sentinel")?;
        std::fs::create_dir(&aliased_data)?;
        symlink(&unrelated, aliased_data.join("bitcoin.conf"))?;
        assert!(ensure_bitcoin_conf(&aliased_data).is_err());
        assert_eq!(std::fs::read(&unrelated)?, b"sentinel");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn bitcoin_conf_creation_rejects_a_symlinked_data_directory() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let real = temporary.path().join("real");
        let alias = temporary.path().join("BitcoinChain");
        std::fs::create_dir(&real)?;
        symlink(&real, &alias)?;

        assert!(ensure_bitcoin_conf(&alias).is_err());
        assert!(!real.join("bitcoin.conf").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rpc_auth_files_are_bounded_and_never_followed() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("target");
        let alias = temporary.path().join("alias");
        std::fs::write(&target, b"rpcuser=outside\nrpcpassword=secret\n")?;
        symlink(&target, &alias)?;
        assert!(read_bounded_regular(&alias, MAX_BITCOIN_CONF_BYTES).is_none());

        let oversized = temporary.path().join("oversized");
        std::fs::write(
            &oversized,
            vec![b'x'; usize::try_from(MAX_COOKIE_BYTES)? + 1],
        )?;
        assert!(read_bounded_regular(&oversized, MAX_COOKIE_BYTES).is_none());
        assert_eq!(
            std::fs::read(&target)?,
            b"rpcuser=outside\nrpcpassword=secret\n"
        );
        Ok(())
    }
}
