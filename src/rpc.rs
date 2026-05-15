//! Bitcoin Core JSON-RPC client.
//!
//! Uses cookie-file authentication by default (the `.cookie` file that
//! `bitcoind` writes on every startup).  Falls back to `rpcuser`/`rpcpassword`
//! from `bitcoin.conf` when no cookie is found.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

        let Ok(text) = std::fs::read_to_string(data_dir.join("bitcoin.conf")) else {
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
            if let Ok(contents) = std::fs::read_to_string(&cookie_path) {
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
    let conf_path = data_dir.join("bitcoin.conf");
    if conf_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create bitcoin data dir {}", data_dir.display()))?;
    std::fs::write(
        &conf_path,
        "# Bitcoin Core — auto-generated by Bitcoin Node Manager\n\
         server=1\n\
         txindex=1\n\
         rpcport=8332\n\
         rpcallowip=127.0.0.1\n\
         # Cookie-based authentication is active by default.\n",
    )
    .with_context(|| format!("write bitcoin.conf {}", conf_path.display()))?;
    Ok(())
}
