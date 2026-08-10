//! Bitcoin Core JSON-RPC client.
//!
//! Managed launches use the exact cookie path and numeric endpoint snapshotted
//! for the active Bitcoin lifecycle generation.

use std::{
    fs::OpenOptions,
    io::{Read as _, Write as _},
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt as _,
    path::Path,
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockchainInfo {
    pub blocks: u64,
    pub headers: u64,
    pub verification_progress: f64,
    pub initial_block_download: bool,
    pub pruned: bool,
}

/// Parsed result of `getnetworkinfo` fields required by managed Electrs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkInfo {
    pub version: u64,
    pub network_active: bool,
}

// ── Authentication ────────────────────────────────────────────────────────────

/// Authentication credentials for Bitcoin RPC.
#[derive(Debug, Clone)]
pub struct RpcAuth {
    pub user: String,
    pub password: String,
    pub endpoint: SocketAddr,
}

impl RpcAuth {
    /// Load the exact cookie and socket owned by the current managed launch.
    pub fn from_managed_cookie(cookie_file: &Path, endpoint: SocketAddr) -> Result<Self> {
        let contents = read_bounded_regular(cookie_file, MAX_COOKIE_BYTES).with_context(|| {
            format!(
                "managed Bitcoin RPC cookie is not available at {}",
                cookie_file.display()
            )
        })?;
        let (user, password) = contents
            .trim()
            .split_once(':')
            .context("managed Bitcoin RPC cookie is malformed")?;
        if user.is_empty() || password.is_empty() {
            bail!("managed Bitcoin RPC cookie is malformed");
        }
        Ok(Self {
            user: user.to_owned(),
            password: password.to_owned(),
            endpoint,
        })
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
    let url = format!("http://{}/", auth.endpoint);

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
    parse_blockchain_info(&v)
}

fn parse_blockchain_info(v: &Value) -> Result<BlockchainInfo> {
    Ok(BlockchainInfo {
        blocks: v["blocks"]
            .as_u64()
            .context("getblockchaininfo blocks was missing or invalid")?,
        headers: v["headers"]
            .as_u64()
            .context("getblockchaininfo headers was missing or invalid")?,
        verification_progress: v["verificationprogress"]
            .as_f64()
            .context("getblockchaininfo verificationprogress was missing or invalid")?,
        initial_block_download: v["initialblockdownload"]
            .as_bool()
            .context("getblockchaininfo initialblockdownload was missing or invalid")?,
        pruned: v["pruned"]
            .as_bool()
            .context("getblockchaininfo pruned was missing or invalid")?,
    })
}

/// Call `getnetworkinfo` and retain the compatibility fields Electrs checks.
pub async fn get_network_info(auth: &RpcAuth) -> Result<NetworkInfo> {
    let v = call(auth, "getnetworkinfo", Value::Array(vec![])).await?;
    parse_network_info(&v)
}

fn parse_network_info(v: &Value) -> Result<NetworkInfo> {
    Ok(NetworkInfo {
        version: v["version"]
            .as_u64()
            .context("getnetworkinfo version was missing or invalid")?,
        network_active: v["networkactive"]
            .as_bool()
            .context("getnetworkinfo networkactive was missing or invalid")?,
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
    use serde_json::json;

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
    fn managed_rpc_auth_uses_only_the_snapshotted_cookie_and_endpoint() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let managed_cookie = temporary.path().join(".cookie");
        let stale_cookie = temporary.path().join("mainnet").join(".cookie");
        std::fs::create_dir(temporary.path().join("mainnet"))?;
        std::fs::write(&managed_cookie, b"managed-user:managed-password\n")?;
        std::fs::write(&stale_cookie, b"stale-user:stale-password\n")?;
        let endpoint: SocketAddr = "[::1]:18443".parse()?;

        let auth = RpcAuth::from_managed_cookie(&managed_cookie, endpoint)?;

        assert_eq!(auth.user, "managed-user");
        assert_eq!(auth.password, "managed-password");
        assert_eq!(auth.endpoint, endpoint);

        let target = temporary.path().join("target-cookie");
        let alias = temporary.path().join("alias-cookie");
        std::fs::write(&target, b"outside:secret\n")?;
        symlink(&target, &alias)?;
        assert!(RpcAuth::from_managed_cookie(&alias, endpoint).is_err());

        let oversized = temporary.path().join("oversized");
        std::fs::write(
            &oversized,
            vec![b'x'; usize::try_from(MAX_COOKIE_BYTES)? + 1],
        )?;
        assert!(RpcAuth::from_managed_cookie(&oversized, endpoint).is_err());
        assert_eq!(std::fs::read(&target)?, b"outside:secret\n");
        Ok(())
    }

    #[test]
    fn electrs_compatibility_fields_are_parsed_fail_closed() -> Result<()> {
        let blockchain = json!({
            "blocks": 100,
            "headers": 100,
            "verificationprogress": 1.0,
            "initialblockdownload": false,
            "pruned": false
        });
        assert_eq!(
            parse_blockchain_info(&blockchain)?,
            BlockchainInfo {
                blocks: 100,
                headers: 100,
                verification_progress: 1.0,
                initial_block_download: false,
                pruned: false,
            }
        );
        let mut missing_ibd = blockchain;
        missing_ibd
            .as_object_mut()
            .context("test blockchain object")?
            .remove("initialblockdownload");
        assert!(parse_blockchain_info(&missing_ibd).is_err());

        let network = json!({"version": 300_000, "networkactive": true});
        assert_eq!(
            parse_network_info(&network)?,
            NetworkInfo {
                version: 300_000,
                network_active: true,
            }
        );
        let missing_network_state = json!({"version": 300_000});
        assert!(parse_network_info(&missing_network_state).is_err());
        Ok(())
    }
}
