//! Bitcoin Core JSON-RPC client.
//!
//! Managed launches use the exact cookie path and numeric endpoint snapshotted
//! for the active Bitcoin lifecycle generation.

// Sizes, counters, protocol values, and timeouts require explicit arithmetic.
#![deny(clippy::arithmetic_side_effects)]

use std::{
    fs::OpenOptions,
    io::{Read as _, Write as _},
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const MAX_COOKIE_BYTES: u64 = 4096;
const MAX_RPC_RESPONSE_BYTES: usize = 1024 * 1024;
const RPC_IN_WARMUP: i64 = -28;

/// Lazily-built HTTP client (one per poll cycle is fine; keep it cheap).
fn http_client() -> Result<Client> {
    Client::builder()
        // Managed RPC credentials belong only to the snapshotted local socket,
        // never to a proxy inherited from the application's environment.
        .no_proxy()
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
    error: Option<RpcResponseError>,
}

#[derive(Debug, Deserialize, Error)]
#[error("RPC error {code}: {message}")]
struct RpcResponseError {
    code: i64,
    message: String,
}

#[derive(Debug, Error)]
enum ManagedCookieError {
    #[error("managed Bitcoin RPC cookie is not available at {}", path.display())]
    Unavailable { path: PathBuf },
    #[error("managed Bitcoin RPC cookie is not a safe regular file at {}", path.display())]
    UnsafeFile { path: PathBuf },
    #[error("could not read managed Bitcoin RPC cookie at {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("managed Bitcoin RPC cookie is malformed")]
    Malformed,
}

#[derive(Debug, Error)]
#[error("RPC HTTP request: {source}")]
struct RpcTransportError {
    #[source]
    source: reqwest::Error,
}

#[derive(Debug, Error)]
#[error("RPC authentication failed (401). Check bitcoin.conf credentials or .cookie file.")]
struct RpcAuthenticationError;

/// Return Bitcoin Core's startup status when an RPC call failed with
/// `RPC_IN_WARMUP` (`-28`).
pub(super) fn rpc_warmup_message(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<RpcResponseError>()
        .filter(|response| response.code == RPC_IN_WARMUP)
        .map(|response| response.message.as_str())
}

/// Return whether an RPC failure is expected while the managed service and its
/// cookie are first becoming reachable. Protocol, authentication, and response
/// validation errors are deliberately excluded.
pub(super) fn is_transient_startup_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RpcTransportError>().is_some()
        || matches!(
            error.downcast_ref::<ManagedCookieError>(),
            Some(ManagedCookieError::Unavailable { .. })
        )
}

/// Parsed result of `getblockchaininfo`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct BlockchainInfo {
    pub(super) blocks: u64,
    pub(super) headers: u64,
    pub(super) verification_progress: f64,
    pub(super) initial_block_download: bool,
    pub(super) pruned: bool,
}

/// Parsed result of `getnetworkinfo` fields required by managed Electrs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct NetworkInfo {
    pub(super) version: u64,
    pub(super) network_active: bool,
}

// ── Authentication ────────────────────────────────────────────────────────────

/// Authentication credentials for Bitcoin RPC.
#[derive(Clone)]
pub(super) struct RpcAuth {
    user: String,
    password: String,
    pub(super) endpoint: SocketAddr,
}

impl std::fmt::Debug for RpcAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcAuth")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl RpcAuth {
    /// Load the exact cookie and socket owned by the current managed launch.
    ///
    /// # Errors
    /// Returns a classified error if the cookie is missing, non-regular,
    /// oversized, unreadable, or malformed. Cookie contents are never reported.
    pub(super) fn from_managed_cookie(cookie_file: &Path, endpoint: SocketAddr) -> Result<Self> {
        let contents = read_bounded_regular(cookie_file, MAX_COOKIE_BYTES)?;
        let (user, password) = contents
            .trim()
            .split_once(':')
            .ok_or(ManagedCookieError::Malformed)?;
        if user.is_empty() || password.is_empty() {
            return Err(ManagedCookieError::Malformed.into());
        }
        Ok(Self {
            user: user.to_owned(),
            password: password.to_owned(),
            endpoint,
        })
    }
}

fn read_bounded_regular(
    path: &Path,
    limit: u64,
) -> std::result::Result<String, ManagedCookieError> {
    let unavailable = || ManagedCookieError::Unavailable {
        path: path.to_path_buf(),
    };
    let unsafe_file = || ManagedCookieError::UnsafeFile {
        path: path.to_path_buf(),
    };
    let read_error = |source| ManagedCookieError::Read {
        path: path.to_path_buf(),
        source,
    };

    let initial = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(unavailable()),
        Err(error) => return Err(read_error(error)),
    };
    if initial.file_type().is_symlink() || !initial.is_file() || initial.len() > limit {
        return Err(unsafe_file());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                unavailable()
            } else {
                read_error(error)
            }
        })?;
    let metadata = file.metadata().map_err(read_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(unsafe_file());
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| unsafe_file())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > limit) {
        return Err(unsafe_file());
    }
    String::from_utf8(bytes).map_err(|_| ManagedCookieError::Malformed)
}

// ── RPC call ─────────────────────────────────────────────────────────────────

/// Make one asynchronous RPC call with a five-second timeout and a 1 MiB body limit.
///
/// # Errors
/// Returns transport, authentication, bounded-response, JSON decoding, or Core
/// RPC errors. Core warmup errors retain their code for startup retry decisions.
pub(super) async fn call(auth: &RpcAuth, method: &str, params: Value) -> Result<Value> {
    let client = http_client()?;
    let url = format!("http://{}/", auth.endpoint);

    let req = RpcRequest {
        jsonrpc: "1.0",
        id: "bnm",
        method,
        params,
    };

    let mut resp = client
        .post(&url)
        .basic_auth(&auth.user, Some(&auth.password))
        .json(&req)
        .send()
        .await
        .map_err(|source| RpcTransportError { source })?;

    let status = resp.status();
    if status == 401 {
        return Err(RpcAuthenticationError.into());
    }

    if resp.content_length().is_some_and(|length| {
        usize::try_from(length).map_or(true, |length| length > MAX_RPC_RESPONSE_BYTES)
    }) {
        bail!("RPC response exceeded {MAX_RPC_RESPONSE_BYTES} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("read RPC response")? {
        if body.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
            bail!("RPC response exceeded {MAX_RPC_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    let rpc_resp: RpcResponse = serde_json::from_slice(&body).context("parse RPC response")?;

    if let Some(err) = rpc_resp.error {
        return Err(err.into());
    }

    rpc_resp.result.context("RPC result was null")
}

/// Call `getblockchaininfo` and return parsed data.
///
/// # Errors
/// Returns an RPC failure or an error for missing or incorrectly typed chain fields.
pub(super) async fn get_blockchain_info(auth: &RpcAuth) -> Result<BlockchainInfo> {
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
///
/// # Errors
/// Returns an RPC failure or an error for missing or incorrectly typed network fields.
pub(super) async fn get_network_info(auth: &RpcAuth) -> Result<NetworkInfo> {
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
///
/// # Errors
/// Returns the underlying RPC failure; callers retain responsibility for shutdown fallback.
pub(super) async fn stop_bitcoind(auth: &RpcAuth) -> Result<()> {
    call(auth, "stop", Value::Array(vec![])).await?;
    Ok(())
}

// ── Default bitcoin.conf generator ───────────────────────────────────────────

/// Create a minimal `bitcoin.conf` if one doesn't exist yet.
///
/// # Errors
/// Returns an error for unsafe paths or any creation, write, or durability failure.
/// An existing regular configuration is preserved.
pub(super) fn ensure_bitcoin_conf(data_dir: &Path) -> Result<()> {
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<()> {
        let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidData);
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            request.extend_from_slice(&buffer[..read]);
            if request.len() > 4096 {
                return Err(invalid());
            }
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).map_err(|_| invalid())?;
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .ok_or_else(invalid)?
                .1
                .trim()
                .parse::<usize>()
                .map_err(|_| invalid())?;
            let total = header_end
                .checked_add(4)
                .and_then(|headers| headers.checked_add(length))
                .filter(|total| *total <= 4096)
                .ok_or_else(invalid)?;
            if request.len() >= total {
                return Ok(());
            }
        }
    }

    async fn mock_rpc_response(
        response: Vec<u8>,
    ) -> Result<(RpcAuth, tokio::task::JoinHandle<std::io::Result<()>>)> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            // Consume the complete request before closing the response side;
            // an unread request body can otherwise cause a TCP reset.
            read_mock_request(&mut stream).await?;
            match stream.write_all(&response).await {
                // A rejected oversized body can close the connection while
                // this fixture is still writing its deliberately invalid data.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    Ok(())
                }
                result => result,
            }
        });
        Ok((
            RpcAuth {
                user: "fixture-user".to_owned(),
                password: "fixture-password".to_owned(),
                endpoint,
            },
            server,
        ))
    }

    #[tokio::test]
    async fn rpc_response_limits_cover_declared_and_streamed_lengths() -> Result<()> {
        let oversized_length = MAX_RPC_RESPONSE_BYTES.saturating_add(1);
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {oversized_length}\r\nConnection: close\r\n\r\n"
        );
        let mut streamed = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{oversized_length:x}\r\n"
        );
        streamed.push_str(&"x".repeat(oversized_length));
        streamed.push_str("\r\n0\r\n\r\n");
        for response in [declared, streamed] {
            let (auth, server) = mock_rpc_response(response.into_bytes()).await?;
            let error = call(&auth, "getblockchaininfo", json!([]))
                .await
                .expect_err("oversized RPC response must fail closed");
            assert!(error.to_string().contains("RPC response exceeded"));
            assert!(!is_transient_startup_error(&error));
            server.await??;
        }
        Ok(())
    }

    #[tokio::test]
    async fn bounded_rpc_responses_preserve_success_warmup_and_authentication_errors() -> Result<()>
    {
        enum Outcome {
            Success,
            Warmup,
            Authentication,
        }
        for (status, body, expected) in [
            (
                "200 OK",
                r#"{"result":42,"error":null,"id":"bnm"}"#,
                Outcome::Success,
            ),
            (
                "500 Internal Server Error",
                r#"{"result":null,"error":{"code":-28,"message":"Loading block index"},"id":"bnm"}"#,
                Outcome::Warmup,
            ),
            ("401 Unauthorized", "", Outcome::Authentication),
        ] {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let (auth, server) = mock_rpc_response(response.into_bytes()).await?;
            let result = call(&auth, "getblockchaininfo", json!([])).await;
            match expected {
                Outcome::Success => assert_eq!(result?, json!(42)),
                Outcome::Warmup => assert_eq!(
                    rpc_warmup_message(&result.expect_err("warmup must remain an error")),
                    Some("Loading block index")
                ),
                Outcome::Authentication => assert!(result
                    .expect_err("authentication must remain an error")
                    .downcast_ref::<RpcAuthenticationError>()
                    .is_some()),
            }
            server.await??;
        }
        Ok(())
    }

    #[tokio::test]
    async fn rpc_timeout_retains_the_transport_error_without_leaking_credentials() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let auth = RpcAuth {
            user: "fixture-user".to_owned(),
            password: "fixture-password".to_owned(),
            endpoint: listener.local_addr()?,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            read_mock_request(&mut stream).await?;
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
            Ok::<(), std::io::Error>(())
        });
        let result = call(&auth, "getblockchaininfo", json!([])).await;
        server.abort();
        assert!(server
            .await
            .expect_err("fixture server was aborted")
            .is_cancelled());
        let error = result.expect_err("an unresponsive RPC peer must time out");
        assert!(is_transient_startup_error(&error));
        assert!(error
            .downcast_ref::<RpcTransportError>()
            .is_some_and(|error| error.source.is_timeout()));
        let diagnostic = format!("{error:#}");
        assert!(!diagnostic.contains(&auth.user));
        assert!(!diagnostic.contains(&auth.password));
        Ok(())
    }

    #[tokio::test]
    async fn managed_rpc_ignores_proxy_environment() -> Result<()> {
        const FIXTURE_ENDPOINT: &str = "BITENGINE_TEST_RPC_PROXY_ENDPOINT";
        if let Ok(endpoint) = std::env::var(FIXTURE_ENDPOINT) {
            let endpoint: SocketAddr = endpoint.parse()?;
            anyhow::ensure!(
                endpoint.ip().is_loopback(),
                "fixture endpoint must be local"
            );
            let auth = RpcAuth {
                user: "fixture-user".to_owned(),
                password: "fixture-password".to_owned(),
                endpoint,
            };
            assert_eq!(
                call(&auth, "getblockchaininfo", json!([])).await?,
                json!(42)
            );
            return Ok(());
        }

        let body = r#"{"result":42,"error":null,"id":"bnm"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (auth, server) = mock_rpc_response(response.into_bytes()).await?;
        let proxy = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        proxy.set_nonblocking(true)?;
        let proxy_url = format!("http://{}", proxy.local_addr()?);
        // Isolate proxy settings in a child test process; global environment
        // mutation would race with the rest of the network fixture suite.
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "rpc::tests::managed_rpc_ignores_proxy_environment",
                ])
                .env(FIXTURE_ENDPOINT, auth.endpoint.to_string())
                .env("HTTP_PROXY", &proxy_url)
                .env("http_proxy", &proxy_url)
                .env("ALL_PROXY", &proxy_url)
                .env("all_proxy", &proxy_url)
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .env_remove("REQUEST_METHOD")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("proxy fixture process timed out")??;
        if !output.status.success() {
            server.abort();
            let _ = server.await;
            bail!(
                "proxy fixture failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        server.await??;
        assert!(
            matches!(proxy.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "managed RPC must never connect to the environment proxy"
        );
        Ok(())
    }

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

        let missing_error =
            RpcAuth::from_managed_cookie(&temporary.path().join("not-created.cookie"), endpoint)
                .unwrap_err();
        assert!(is_transient_startup_error(&missing_error));

        let auth = RpcAuth::from_managed_cookie(&managed_cookie, endpoint)?;

        assert_eq!(auth.user, "managed-user");
        assert_eq!(auth.password, "managed-password");
        assert_eq!(auth.endpoint, endpoint);
        let diagnostic = format!("{auth:?}");
        assert!(diagnostic.contains("[::1]:18443"));
        assert!(!diagnostic.contains("managed-user"));
        assert!(!diagnostic.contains("managed-password"));

        let target = temporary.path().join("target-cookie");
        let alias = temporary.path().join("alias-cookie");
        std::fs::write(&target, b"outside:secret\n")?;
        symlink(&target, &alias)?;
        let symlink_error = RpcAuth::from_managed_cookie(&alias, endpoint).unwrap_err();
        assert!(!is_transient_startup_error(&symlink_error));

        let oversized = temporary.path().join("oversized");
        std::fs::write(
            &oversized,
            vec![b'x'; usize::try_from(MAX_COOKIE_BYTES)? + 1],
        )?;
        let oversized_error = RpcAuth::from_managed_cookie(&oversized, endpoint).unwrap_err();
        assert!(!is_transient_startup_error(&oversized_error));
        let malformed = temporary.path().join("malformed");
        std::fs::write(&malformed, b"missing-separator")?;
        let malformed_error = RpcAuth::from_managed_cookie(&malformed, endpoint).unwrap_err();
        assert!(!is_transient_startup_error(&malformed_error));
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
