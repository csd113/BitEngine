use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time::timeout,
};

use crate::{
    config::Config,
    rpc::{self, BlockchainInfo, RpcAuth},
};

const ELECTRUM_PROTOCOL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ELECTRUM_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_METRICS_RESPONSE_BYTES: usize = 1024 * 1024;
const VERSION_REQUEST_ID: &str = "bitengine-version";
const FEATURES_REQUEST_ID: &str = "bitengine-features";
const ELECTRUM_PROTOCOL_VERSION: &str = "1.4";
const BITCOIN_MAINNET_GENESIS_HASH: &str =
    "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
const ELECTRUM_PROBE_REQUEST: &str = concat!(
    r#"[{"jsonrpc":"2.0","id":"bitengine-version","method":"server.version","params":["BitEngine","1.4"]},{"jsonrpc":"2.0","id":"bitengine-features","method":"server.features","params":[]}]"#,
    "\n"
);

#[derive(Debug, Clone, Default, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "process, dependency connection, index synchronization, and service readiness are independent protocol invariants"
)]
pub struct ElectrsStatus {
    pub running: bool,
    pub connected: bool,
    pub synced: bool,
    pub ready: bool,
    pub electrs_height: Option<u64>,
    pub bitcoin_blocks: Option<u64>,
    pub bitcoin_headers: Option<u64>,
    pub sync_percent: Option<f64>,
    pub metrics_error: Option<String>,
    pub bitcoin_error: Option<String>,
    pub connect_error: Option<String>,
}

pub async fn probe(
    process_running: bool,
    managed_bitcoin_rpc: Option<(PathBuf, SocketAddr)>,
) -> ElectrsStatus {
    if !process_running {
        return ElectrsStatus::default();
    }

    let metrics_url = Config::electrs_metrics_url();
    let electrum_addr = Config::electrum_addr().to_owned();
    let bitcoin_probe = async move {
        let (cookie_file, endpoint) =
            managed_bitcoin_rpc.context("managed Bitcoin RPC endpoint snapshot is unavailable")?;
        let auth = RpcAuth::from_managed_cookie(&cookie_file, endpoint)?;
        rpc::get_blockchain_info(&auth).await
    };

    let (metrics_result, bitcoin_result, protocol_result) = tokio::join!(
        fetch_metrics(&metrics_url),
        bitcoin_probe,
        check_electrum_protocol(&electrum_addr),
    );

    build_status(
        process_running,
        metrics_result.map_err(|e| e.to_string()),
        bitcoin_result.map_err(|e| e.to_string()),
        protocol_result.map_err(|e| e.to_string()),
    )
}

async fn fetch_metrics(url: &str) -> Result<String> {
    let mut response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build metrics HTTP client")?
        .get(url)
        .send()
        .await
        .context("request electrs metrics")?;

    let status = response.status();
    if status != StatusCode::OK {
        anyhow::bail!("metrics endpoint returned HTTP {status}");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read electrs metrics body")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_METRICS_RESPONSE_BYTES {
            anyhow::bail!("electrs metrics response exceeded {MAX_METRICS_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).context("electrs metrics body was not UTF-8")
}

fn build_status(
    process_running: bool,
    metrics_result: Result<String, String>,
    bitcoin_result: Result<BlockchainInfo, String>,
    protocol_result: Result<ElectrumProtocolStatus, String>,
) -> ElectrsStatus {
    if !process_running {
        return ElectrsStatus::default();
    }

    let (electrs_height, metrics_error) = match metrics_result {
        Ok(metrics) => match parse_tip_height(&metrics) {
            Ok(height) => (Some(height), None),
            Err(err) => (None, Some(err)),
        },
        Err(err) => (None, Some(format!("metrics unreachable: {err}"))),
    };

    let (bitcoin_blocks, bitcoin_headers, bitcoin_chain_synced, bitcoin_error) =
        match bitcoin_result {
            Ok(info) => (
                Some(info.blocks),
                Some(info.headers),
                !info.initial_block_download && info.blocks >= info.headers,
                None,
            ),
            Err(err) => (
                None,
                None,
                false,
                Some(format!("Bitcoin Core unavailable: {err}")),
            ),
        };

    let sync_percent = match (electrs_height, bitcoin_blocks) {
        (Some(index_height), Some(blocks)) if blocks > 0 => u32::try_from(index_height)
            .ok()
            .zip(u32::try_from(blocks).ok())
            .map(|(index_height, blocks)| {
                (f64::from(index_height) / f64::from(blocks) * 100.0).min(100.0)
            }),
        _ => None,
    };

    let (connected, ready, connect_error) = match protocol_result {
        Ok(status) => (
            status.connected,
            status.connected && status.ready,
            status.error,
        ),
        Err(error) => (
            false,
            false,
            Some(format!("Electrum protocol unavailable: {error}")),
        ),
    };

    let synced = connected
        && bitcoin_chain_synced
        && matches!(
            (electrs_height, bitcoin_blocks),
            (Some(index_height), Some(blocks)) if index_height >= blocks
        )
        && metrics_error.is_none()
        && bitcoin_error.is_none();

    ElectrsStatus {
        running: true,
        connected,
        synced,
        ready,
        electrs_height,
        bitcoin_blocks,
        bitcoin_headers,
        sync_percent,
        metrics_error,
        bitcoin_error,
        connect_error,
    }
}

pub fn parse_tip_height(metrics: &str) -> Result<u64, String> {
    let mut matching_lines = metrics
        .lines()
        .filter(|line| line.starts_with("electrs_index_height{") && line.contains("type=\"tip\""));

    let Some(line) = matching_lines.next() else {
        return Err("electrs tip height metric missing".to_owned());
    };
    if matching_lines.next().is_some() {
        return Err("electrs tip height metric was duplicated".to_owned());
    }

    let Some(raw_value) = line.split_whitespace().last() else {
        return Err("electrs tip height metric malformed".to_owned());
    };

    raw_value
        .parse::<u64>()
        .map_err(|_| "electrs tip height metric malformed".to_owned())
}

#[derive(Debug, PartialEq, Eq)]
struct ElectrumProtocolStatus {
    connected: bool,
    ready: bool,
    error: Option<String>,
}

async fn check_electrum_protocol(addr: &str) -> Result<ElectrumProtocolStatus> {
    timeout(ELECTRUM_PROTOCOL_TIMEOUT, async {
        let mut stream = TcpStream::connect(addr)
            .await
            .context("electrum TCP connect failed")?;
        stream
            .write_all(ELECTRUM_PROBE_REQUEST.as_bytes())
            .await
            .context("write Electrum protocol probe")?;
        stream
            .flush()
            .await
            .context("flush Electrum protocol probe")?;

        let response = read_bounded_protocol_response(&mut stream).await?;
        parse_protocol_response(&response)
    })
    .await
    .context("electrum protocol probe timed out")?
}

async fn read_bounded_protocol_response(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut response = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];

    loop {
        let bytes_read = stream
            .read(&mut buffer)
            .await
            .context("read Electrum protocol response")?;
        if bytes_read == 0 {
            anyhow::bail!("Electrum server closed before a newline-delimited response");
        }

        let newline = buffer[..bytes_read].iter().position(|byte| *byte == b'\n');
        let response_bytes = newline.unwrap_or(bytes_read);
        if response.len().saturating_add(response_bytes) > MAX_ELECTRUM_RESPONSE_BYTES {
            anyhow::bail!(
                "Electrum protocol response exceeded {MAX_ELECTRUM_RESPONSE_BYTES} bytes"
            );
        }
        response.extend_from_slice(&buffer[..response_bytes]);

        if newline.is_some() {
            return Ok(response);
        }
    }
}

fn parse_protocol_response(response: &[u8]) -> Result<ElectrumProtocolStatus> {
    let value: Value =
        serde_json::from_slice(response).context("parse Electrum protocol response")?;
    let responses = value
        .as_array()
        .context("Electrum protocol response was not a JSON batch")?;

    let version_response = response_by_id(responses, VERSION_REQUEST_ID)?
        .context("Electrum response omitted server.version")?;
    validate_response_envelope(version_response, VERSION_REQUEST_ID)?;
    validate_version_response(version_response)?;

    let features_response = match response_by_id(responses, FEATURES_REQUEST_ID) {
        Ok(Some(response)) => response,
        Ok(None) => {
            return Ok(ElectrumProtocolStatus {
                connected: true,
                ready: false,
                error: Some("Electrum response omitted server.features".to_owned()),
            });
        }
        Err(error) => {
            return Ok(ElectrumProtocolStatus {
                connected: true,
                ready: false,
                error: Some(error.to_string()),
            });
        }
    };

    match validate_features_response(features_response) {
        Ok(()) => Ok(ElectrumProtocolStatus {
            connected: true,
            ready: true,
            error: None,
        }),
        Err(error) => Ok(ElectrumProtocolStatus {
            connected: true,
            ready: false,
            error: Some(error.to_string()),
        }),
    }
}

fn response_by_id<'a>(responses: &'a [Value], id: &str) -> Result<Option<&'a Value>> {
    let mut matches = responses
        .iter()
        .filter(|response| response.get("id").and_then(Value::as_str) == Some(id));
    let response = matches.next();
    if matches.next().is_some() {
        anyhow::bail!("Electrum response duplicated request id {id}");
    }
    Ok(response)
}

fn validate_response_envelope(response: &Value, expected_id: &str) -> Result<()> {
    let object = response
        .as_object()
        .context("Electrum batch item was not a JSON object")?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        anyhow::bail!("Electrum response did not identify JSON-RPC 2.0");
    }
    if object.get("id").and_then(Value::as_str) != Some(expected_id) {
        anyhow::bail!("Electrum response id did not match {expected_id}");
    }
    Ok(())
}

fn validate_version_response(response: &Value) -> Result<()> {
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        anyhow::bail!("server.version returned an error: {error}");
    }

    let result = response
        .get("result")
        .and_then(Value::as_array)
        .context("server.version result was not an array")?;
    let [server, protocol] = result.as_slice() else {
        anyhow::bail!("server.version result had an unexpected shape");
    };
    let server = server
        .as_str()
        .context("server.version omitted the server identity")?;
    let protocol = protocol
        .as_str()
        .context("server.version omitted the protocol version")?;
    if !identifies_electrs(server) {
        anyhow::bail!("server.version did not identify electrs");
    }
    if protocol != ELECTRUM_PROTOCOL_VERSION {
        anyhow::bail!("server.version reported unsupported Electrum protocol {protocol}");
    }
    Ok(())
}

fn validate_features_response(response: &Value) -> Result<()> {
    validate_response_envelope(response, FEATURES_REQUEST_ID)?;
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        if is_unavailable_index_error(error) {
            anyhow::bail!("Electrs index is not ready (server.features: unavailable index)");
        }
        anyhow::bail!("server.features returned an error: {error}");
    }

    let features = response
        .get("result")
        .and_then(Value::as_object)
        .context("server.features result was not an object")?;
    let server = features
        .get("server_version")
        .and_then(Value::as_str)
        .context("server.features omitted the server identity")?;
    if !identifies_electrs(server) {
        anyhow::bail!("server.features did not identify electrs");
    }
    let genesis_hash = features
        .get("genesis_hash")
        .and_then(Value::as_str)
        .context("server.features omitted the genesis hash")?;
    if genesis_hash != BITCOIN_MAINNET_GENESIS_HASH {
        anyhow::bail!("server.features did not identify Bitcoin mainnet");
    }
    Ok(())
}

fn identifies_electrs(server: &str) -> bool {
    server
        .strip_prefix("electrs/")
        .is_some_and(|version| !version.is_empty())
}

fn is_unavailable_index_error(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(-32_603)
        && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.eq_ignore_ascii_case("unavailable index"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    use super::{
        build_status, check_electrum_protocol, parse_protocol_response, parse_tip_height, probe,
        ElectrsStatus, ElectrumProtocolStatus, BITCOIN_MAINNET_GENESIS_HASH,
        ELECTRUM_PROTOCOL_TIMEOUT, FEATURES_REQUEST_ID, MAX_ELECTRUM_RESPONSE_BYTES,
        VERSION_REQUEST_ID,
    };
    use crate::rpc::BlockchainInfo;

    fn blockchain_info(blocks: u64, headers: u64) -> BlockchainInfo {
        BlockchainInfo {
            blocks,
            headers,
            verification_progress: 1.0,
            initial_block_download: false,
            pruned: false,
        }
    }

    fn connected_protocol(ready: bool) -> ElectrumProtocolStatus {
        ElectrumProtocolStatus {
            connected: true,
            ready,
            error: (!ready).then(|| "Electrs index is not ready".to_owned()),
        }
    }

    fn protocol_response(features_response: &Value) -> String {
        format!(
            "{}\n",
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": VERSION_REQUEST_ID,
                    "result": ["electrs/0.11.1", "1.4"]
                },
                features_response
            ])
        )
    }

    fn ready_protocol_response() -> String {
        protocol_response(&json!({
            "jsonrpc": "2.0",
            "id": FEATURES_REQUEST_ID,
            "result": {
                "server_version": "electrs/0.11.1",
                "genesis_hash": BITCOIN_MAINNET_GENESIS_HASH
            }
        }))
    }

    async fn spawn_protocol_server(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept protocol probe");
            let mut reader = tokio::io::BufReader::new(stream);
            let mut request = String::new();
            reader
                .read_line(&mut request)
                .await
                .expect("read protocol probe");
            let mut stream = reader.into_inner();
            let _ = stream.write_all(response.as_bytes()).await;
            request
        });
        (addr.to_string(), server)
    }

    #[test]
    fn parses_tip_height_metric() {
        let metrics = "# HELP electrs_index_height tip height\n\
electrs_index_height{type=\"tip\"} 856201\n";

        assert_eq!(parse_tip_height(metrics), Ok(856_201));
        assert_eq!(
            parse_tip_height(
                "electrs_index_height{type=\"tip\"} 1\n\
                 electrs_index_height{type=\"tip\"} 2\n"
            ),
            Err("electrs tip height metric was duplicated".to_owned())
        );
    }

    #[test]
    fn reports_synced_when_heights_match() {
        let status = build_status(
            true,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Ok(blockchain_info(856_201, 856_201)),
            Ok(connected_protocol(true)),
        );

        assert!(status.running);
        assert!(status.connected);
        assert!(status.synced);
        assert!(status.ready);
        assert_eq!(status.sync_percent, Some(100.0));
    }

    #[test]
    fn matching_heights_without_a_protocol_connection_are_not_synced() {
        let status = build_status(
            true,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Ok(blockchain_info(856_201, 856_201)),
            Err("connection refused".to_owned()),
        );

        assert!(status.running);
        assert!(!status.connected);
        assert!(!status.synced);
        assert!(!status.ready);
        assert!(status.connect_error.is_some());
    }

    #[test]
    fn reports_not_synced_when_electrs_lags() {
        let status = build_status(
            true,
            Ok("electrs_index_height{type=\"tip\"} 856100".to_owned()),
            Ok(blockchain_info(856_201, 856_205)),
            Ok(connected_protocol(true)),
        );

        assert!(!status.synced);
        assert_eq!(status.electrs_height, Some(856_100));
        assert_eq!(status.bitcoin_blocks, Some(856_201));
        assert!(status.sync_percent.is_some_and(|percent| percent < 100.0));
    }

    #[test]
    fn matching_index_height_does_not_hide_bitcoin_ibd_or_header_lag() {
        let matching_metrics = Ok("electrs_index_height{type=\"tip\"} 100".to_owned());
        let header_lag = build_status(
            true,
            matching_metrics.clone(),
            Ok(blockchain_info(100, 101)),
            Ok(connected_protocol(true)),
        );
        assert!(!header_lag.synced);

        let mut ibd = blockchain_info(100, 100);
        ibd.initial_block_download = true;
        let initial_download = build_status(
            true,
            matching_metrics,
            Ok(ibd),
            Ok(connected_protocol(true)),
        );
        assert!(!initial_download.synced);
    }

    #[test]
    fn handles_missing_or_malformed_metrics_without_panicking() {
        let missing = build_status(
            true,
            Ok("# no electrs tip metric here".to_owned()),
            Ok(blockchain_info(100, 100)),
            Ok(connected_protocol(true)),
        );
        assert!(!missing.synced);
        assert!(missing.metrics_error.is_some());

        let malformed = build_status(
            true,
            Ok("electrs_index_height{type=\"tip\"} nope".to_owned()),
            Ok(blockchain_info(100, 100)),
            Ok(connected_protocol(true)),
        );
        assert!(!malformed.synced);
        assert!(malformed.metrics_error.is_some());

        let unreachable = build_status(
            true,
            Err("connection reset".to_owned()),
            Ok(blockchain_info(100, 100)),
            Ok(connected_protocol(true)),
        );
        assert!(!unreachable.synced);
        assert!(unreachable.metrics_error.is_some());
    }

    #[test]
    fn handles_bitcoin_unavailable_without_panicking() {
        let status = build_status(
            true,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Err("rpc auth failed".to_owned()),
            Ok(connected_protocol(true)),
        );

        assert!(!status.synced);
        assert!(status.bitcoin_error.is_some());
        assert_eq!(status.sync_percent, None);
    }

    #[tokio::test]
    async fn tcp_accept_without_a_protocol_response_is_not_connected_or_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let accept_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept protocol probe");
            tokio::time::sleep(ELECTRUM_PROTOCOL_TIMEOUT + Duration::from_millis(50)).await;
            drop(stream);
        });

        let status = build_status(
            true,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            check_electrum_protocol(&addr.to_string())
                .await
                .map_err(|e| e.to_string()),
        );

        assert!(status.running);
        assert!(!status.connected);
        assert!(!status.ready);
        assert!(status
            .connect_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")));
        accept_task.await.expect("accept task join");
    }

    #[tokio::test]
    async fn valid_version_and_mainnet_features_mark_connected_and_ready() {
        let (addr, server) = spawn_protocol_server(ready_protocol_response()).await;

        let protocol = check_electrum_protocol(&addr)
            .await
            .expect("valid Electrum response");
        assert_eq!(protocol, connected_protocol(true));

        let status = build_status(
            true,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            Ok(protocol),
        );
        assert!(status.connected);
        assert!(status.ready);
        assert!(!status.synced);

        let request = server.await.expect("protocol server join");
        assert!(request.ends_with('\n'));
        let request: Value =
            serde_json::from_str(request.trim_end()).expect("parse captured protocol request");
        let batch = request.as_array().expect("protocol request batch");
        assert_eq!(batch.len(), 2);
        assert!(batch.iter().any(|item| {
            item.get("id").and_then(Value::as_str) == Some(VERSION_REQUEST_ID)
                && item.get("method").and_then(Value::as_str) == Some("server.version")
        }));
        assert!(batch.iter().any(|item| {
            item.get("id").and_then(Value::as_str) == Some(FEATURES_REQUEST_ID)
                && item.get("method").and_then(Value::as_str) == Some("server.features")
        }));
    }

    #[test]
    fn unavailable_index_is_connected_but_not_ready() {
        let response = protocol_response(&json!({
            "jsonrpc": "2.0",
            "id": FEATURES_REQUEST_ID,
            "error": {"code": -32603, "message": "unavailable index"}
        }));

        let protocol =
            parse_protocol_response(response.trim_end().as_bytes()).expect("valid response");

        assert!(protocol.connected);
        assert!(!protocol.ready);
        assert!(protocol
            .error
            .as_deref()
            .is_some_and(|error| error.contains("index is not ready")));
    }

    #[test]
    fn wrong_network_is_connected_but_not_ready() {
        let response = protocol_response(&json!({
            "jsonrpc": "2.0",
            "id": FEATURES_REQUEST_ID,
            "result": {
                "server_version": "electrs/0.11.1",
                "genesis_hash": "000000000933ea01ad0ee984209779baae8c49c19d4788cc5b53d8f75f85abf2"
            }
        }));

        let protocol =
            parse_protocol_response(response.trim_end().as_bytes()).expect("valid response");

        assert!(protocol.connected);
        assert!(!protocol.ready);
        assert!(protocol
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Bitcoin mainnet")));
    }

    #[test]
    fn unrelated_service_cannot_claim_an_electrs_connection() {
        let response = format!(
            "{}\n",
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": VERSION_REQUEST_ID,
                    "result": ["other-server/1.0", "1.4"]
                },
                {
                    "jsonrpc": "2.0",
                    "id": FEATURES_REQUEST_ID,
                    "result": {
                        "server_version": "other-server/1.0",
                        "genesis_hash": BITCOIN_MAINNET_GENESIS_HASH
                    }
                }
            ])
        );

        let error = parse_protocol_response(response.trim_end().as_bytes())
            .expect_err("unrelated service must be rejected");

        assert!(error.to_string().contains("did not identify electrs"));
    }

    #[test]
    fn uncorrelated_response_cannot_claim_an_electrs_connection() {
        let response = format!(
            "{}\n",
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": "some-other-version-request",
                    "result": ["electrs/0.11.1", "1.4"]
                },
                {
                    "jsonrpc": "2.0",
                    "id": FEATURES_REQUEST_ID,
                    "result": {
                        "server_version": "electrs/0.11.1",
                        "genesis_hash": BITCOIN_MAINNET_GENESIS_HASH
                    }
                }
            ])
        );

        let error = parse_protocol_response(response.trim_end().as_bytes())
            .expect_err("uncorrelated response must be rejected");

        assert!(error.to_string().contains("omitted server.version"));
    }

    #[tokio::test]
    async fn protocol_response_size_is_bounded() {
        let response = format!("{}\n", "x".repeat(MAX_ELECTRUM_RESPONSE_BYTES + 1));
        let (addr, server) = spawn_protocol_server(response).await;

        let error = check_electrum_protocol(&addr)
            .await
            .expect_err("oversized response must be rejected");

        assert!(error.to_string().contains("exceeded"));
        drop(server.await.expect("protocol server join"));
    }

    #[test]
    fn running_is_true_from_process_check_even_if_metrics_fail() {
        let status = build_status(
            true,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            Err("connection refused".to_owned()),
        );

        assert!(status.running);
        assert!(!status.connected);
        assert!(!status.synced);
        assert!(!status.ready);
    }

    #[test]
    fn process_absence_cannot_create_any_runtime_readiness() {
        let status = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Ok(blockchain_info(856_201, 856_201)),
            Ok(connected_protocol(true)),
        );

        assert_eq!(status, ElectrsStatus::default());
    }

    #[tokio::test]
    async fn probe_is_quiet_when_electrs_process_is_not_running() {
        let status = probe(false, None).await;

        assert_eq!(status, ElectrsStatus::default());
    }
}
