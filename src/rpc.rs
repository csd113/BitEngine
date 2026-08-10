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
const MAX_INCLUDED_CONF_FILES: usize = 32;

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

#[derive(Default)]
struct RpcConfigValues {
    port: Option<u16>,
    port_seen: bool,
    user: Option<String>,
    user_seen: bool,
    password: Option<String>,
    password_seen: bool,
}

impl RpcConfigValues {
    fn apply_first(&mut self, name: &str, value: &str) {
        match name {
            "rpcport" if !self.port_seen => {
                self.port_seen = true;
                self.port = value.parse().ok();
            }
            "rpcuser" if !self.user_seen => {
                self.user_seen = true;
                self.user = Some(value.to_owned());
            }
            "rpcpassword" if !self.password_seen => {
                self.password_seen = true;
                self.password = Some(value.to_owned());
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Global,
    Main,
    Other,
}

impl BitcoinConf {
    fn load(data_dir: &Path) -> Self {
        let mut conf = Self {
            port: None,
            rpcuser: None,
            rpcpassword: None,
            cookie_credentials: None,
        };

        let mut global = RpcConfigValues::default();
        let mut mainnet = RpcConfigValues::default();
        let mut included_paths = Vec::new();
        if let Some(text) =
            read_bounded_regular(&data_dir.join("bitcoin.conf"), MAX_BITCOIN_CONF_BYTES)
        {
            parse_rpc_config(&text, &mut global, &mut mainnet, Some(&mut included_paths));
        }
        for included_path in included_paths {
            let included_path = if included_path.is_absolute() {
                included_path
            } else {
                data_dir.join(included_path)
            };
            if let Some(text) = read_bounded_regular(&included_path, MAX_BITCOIN_CONF_BYTES) {
                // Bitcoin Core only honors includeconf in the primary file;
                // nested include directives are intentionally ignored.
                parse_rpc_config(&text, &mut global, &mut mainnet, None);
            }
        }

        conf.port = mainnet.port.or(global.port);
        conf.rpcuser = mainnet.user.or(global.user);
        conf.rpcpassword = mainnet.password.or(global.password);

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

fn parse_rpc_config(
    text: &str,
    global: &mut RpcConfigValues,
    mainnet: &mut RpcConfigValues,
    mut included_paths: Option<&mut Vec<std::path::PathBuf>>,
) {
    let mut scope = ConfigScope::Global;
    for raw_line in text.lines() {
        let line = raw_line
            .split_once('#')
            .map_or(raw_line, |(value, _)| value)
            .trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|section| section.strip_suffix(']'))
        {
            // BitEngine launches both managed nodes on mainnet. Ignore
            // settings from testnet, signet, regtest, and unknown sections.
            scope = if section == "main" {
                ConfigScope::Main
            } else {
                ConfigScope::Other
            };
            continue;
        }

        let Some((raw_name, raw_value)) = line.split_once('=') else {
            continue;
        };
        let raw_name = raw_name.trim();
        let value = raw_value.trim();
        let (target_scope, name) =
            raw_name
                .split_once('.')
                .map_or((scope, raw_name), |(section, name)| {
                    (
                        if section == "main" {
                            ConfigScope::Main
                        } else {
                            ConfigScope::Other
                        },
                        name.trim(),
                    )
                });
        match target_scope {
            ConfigScope::Global | ConfigScope::Main if name == "includeconf" => {
                if let Some(paths) = included_paths.as_deref_mut() {
                    if paths.len() < MAX_INCLUDED_CONF_FILES {
                        paths.push(value.into());
                    }
                }
            }
            ConfigScope::Global => global.apply_first(name, value),
            ConfigScope::Main => mainnet.apply_first(name, value),
            ConfigScope::Other => {}
        }
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

    #[test]
    fn rpc_auth_uses_only_global_and_mainnet_config_sections() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        std::fs::write(
            temporary.path().join("bitcoin.conf"),
            "rpcport=8334\n\
             rpcuser=global-user\n\
             [main]\n\
             rpcport=8335\n\
             rpcuser=main-user\n\
             rpcpassword=main-password\n\
             [test]\n\
             rpcport=18333\n\
             rpcuser=test-user\n\
             rpcpassword=test-password\n",
        )?;

        let auth = RpcAuth::from_data_dir(temporary.path());

        assert_eq!(auth.port, 8335);
        assert_eq!(auth.user, "main-user");
        assert_eq!(auth.password, "main-password");
        Ok(())
    }

    #[test]
    fn rpc_auth_recognizes_mainnet_section_after_other_networks() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        std::fs::write(
            temporary.path().join("bitcoin.conf"),
            "[test]\n\
             rpcport=18333\n\
             [main]\n\
             rpcport=8336\n",
        )?;

        assert_eq!(RpcAuth::from_data_dir(temporary.path()).port, 8336);
        Ok(())
    }

    #[test]
    fn rpc_auth_handles_commented_sections_without_accepting_non_core_section_names() -> Result<()>
    {
        let temporary = tempfile::tempdir()?;
        std::fs::write(
            temporary.path().join("bitcoin.conf"),
            "rpcport=8337\n\
             [test] # testnet settings\n\
             rpcport=18333\n\
             [MAIN]\n\
             rpcport=19999\n",
        )?;

        assert_eq!(RpcAuth::from_data_dir(temporary.path()).port, 8337);
        Ok(())
    }

    #[test]
    fn rpc_auth_supports_whitespace_and_first_mainnet_dotted_assignment() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        std::fs::write(
            temporary.path().join("bitcoin.conf"),
            "rpcport = 8338\n\
             main.rpcport = 8339\n\
             main.rpcport = 8340\n\
             main.rpcuser = first-user\n\
             main.rpcuser = second-user\n",
        )?;

        let auth = RpcAuth::from_data_dir(temporary.path());
        assert_eq!(auth.port, 8339);
        assert_eq!(auth.user, "first-user");
        Ok(())
    }

    #[test]
    fn rpc_auth_reads_mainnet_values_from_bounded_primary_includes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        std::fs::write(
            temporary.path().join("bitcoin.conf"),
            "[main]\n\
             includeconf=rpc-port.conf\n\
             main.includeconf=rpc-auth.conf\n",
        )?;
        std::fs::write(
            temporary.path().join("rpc-port.conf"),
            "includeconf=ignored-nested.conf\n\
             [main]\n\
             rpcport=8341\n",
        )?;
        std::fs::write(
            temporary.path().join("rpc-auth.conf"),
            "[main]\n\
             rpcuser=included-user\n\
             rpcpassword=included-password\n",
        )?;
        std::fs::write(
            temporary.path().join("ignored-nested.conf"),
            "[main]\nrpcport=19999\n",
        )?;

        let auth = RpcAuth::from_data_dir(temporary.path());
        assert_eq!(auth.port, 8341);
        assert_eq!(auth.user, "included-user");
        assert_eq!(auth.password, "included-password");
        Ok(())
    }
}
