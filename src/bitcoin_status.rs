//! Protocol-level health checks for the managed Bitcoin P2P service.

use std::{
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context as _, Result};
use ring::{
    digest::{digest, SHA256},
    rand::{SecureRandom as _, SystemRandom},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time::{timeout, timeout_at, Instant},
};

const MAINNET_MAGIC: [u8; 4] = [0xf9, 0xbe, 0xb4, 0xd9];
const PROTOCOL_VERSION: i32 = 70_016;
const MINIMUM_SUPPORTED_PROTOCOL_VERSION: i32 = 70_001;
const MESSAGE_HEADER_BYTES: usize = 24;
const MAX_HANDSHAKE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_HANDSHAKE_MESSAGES: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct ProbeTimeouts {
    connect: Duration,
    handshake: Duration,
}

const DEFAULT_TIMEOUTS: ProbeTimeouts = ProbeTimeouts {
    connect: CONNECT_TIMEOUT,
    handshake: HANDSHAKE_TIMEOUT,
};

/// Return the first configured endpoint that completes a bounded mainnet
/// `version`/`verack` exchange in deterministic candidate order.
pub async fn probe_p2p_candidates(candidates: &[SocketAddr]) -> Result<SocketAddr> {
    probe_p2p_candidates_with_timeouts(candidates, DEFAULT_TIMEOUTS).await
}

async fn probe_p2p_candidates_with_timeouts(
    candidates: &[SocketAddr],
    timeouts: ProbeTimeouts,
) -> Result<SocketAddr> {
    let mut failures = Vec::with_capacity(candidates.len());
    for &candidate in candidates {
        match probe_p2p(candidate, timeouts).await {
            Ok(()) => return Ok(candidate),
            Err(error) => failures.push(format!("{candidate} — {error:#}")),
        }
    }

    let details = if failures.is_empty() {
        "no candidate endpoints were configured".to_owned()
    } else {
        failures.join("; ")
    };
    bail!("no configured Bitcoin P2P endpoint completed a mainnet handshake: {details}")
}

async fn probe_p2p(candidate: SocketAddr, timeouts: ProbeTimeouts) -> Result<()> {
    let mut stream = match timeout(timeouts.connect, TcpStream::connect(candidate)).await {
        Err(_) => bail!("TCP connection timed out"),
        Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
            bail!("TCP connection refused")
        }
        Ok(Err(error)) if error.kind() == ErrorKind::TimedOut => {
            bail!("TCP connection timed out")
        }
        Ok(Err(error)) => return Err(error).context("TCP connection failed"),
        Ok(Ok(stream)) => stream,
    };
    let version_message = version_message(candidate)?;
    stream
        .write_all(&version_message)
        .await
        .context("connected, failed to send version")?;

    let deadline = Instant::now() + timeouts.handshake;
    let mut saw_version = false;
    let mut saw_verack = false;
    for _ in 0..MAX_HANDSHAKE_MESSAGES {
        let expected = if saw_version { "verack" } else { "version" };
        let message = match timeout_at(deadline, read_message(&mut stream)).await {
            Err(_) => bail!("timed out waiting for {expected}"),
            Ok(Err(error)) if peer_closed(&error) => {
                bail!("connected, peer closed before {expected}")
            }
            Ok(Err(error)) => {
                return Err(error).with_context(|| format!("while waiting for {expected}"));
            }
            Ok(Ok(message)) => message,
        };
        match message.command.as_str() {
            "version" if !saw_version => {
                validate_version(&message.payload)?;
                let verack_message = bitcoin_message("verack", &[], MAINNET_MAGIC)?;
                stream
                    .write_all(&verack_message)
                    .await
                    .context("connected, failed to send verack")?;
                saw_version = true;
                if saw_verack {
                    return Ok(());
                }
            }
            "verack" => {
                if !message.payload.is_empty() {
                    bail!("verack response was malformed: payload was not empty");
                }
                saw_verack = true;
                if saw_version {
                    return Ok(());
                }
            }
            _ => {}
        }
    }

    let expected = if saw_version { "verack" } else { "version" };
    bail!("peer sent {MAX_HANDSHAKE_MESSAGES} messages without completing {expected}")
}

fn validate_version(payload: &[u8]) -> Result<()> {
    if payload.len() < 80 {
        bail!(
            "version response was malformed: payload was {} bytes, expected at least 80",
            payload.len()
        );
    }
    let version_bytes: [u8; 4] = payload[..4].try_into().context("read protocol version")?;
    let version = i32::from_le_bytes(version_bytes);
    if version < MINIMUM_SUPPORTED_PROTOCOL_VERSION {
        bail!("protocol version {version} is unsupported");
    }
    Ok(())
}

fn peer_closed(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            ErrorKind::UnexpectedEof
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionReset
                | ErrorKind::BrokenPipe
        )
    })
}

fn version_message(candidate: SocketAddr) -> Result<Vec<u8>> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    let timestamp = i64::try_from(timestamp.as_secs()).context("system time does not fit i64")?;
    let mut nonce = [0_u8; 8];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("operating-system randomness is unavailable for the version nonce"))?;

    let user_agent = format!("/BitEngine:{}/", env!("CARGO_PKG_VERSION"));
    let user_agent_len = u8::try_from(user_agent.len())
        .context("BitEngine user agent is too long for CompactSize encoding")?;
    if user_agent_len >= 253 {
        bail!("BitEngine user agent is too long for single-byte CompactSize encoding");
    }

    let mut payload = Vec::with_capacity(128);
    payload.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    payload.extend_from_slice(&0_u64.to_le_bytes());
    payload.extend_from_slice(&timestamp.to_le_bytes());
    write_network_address(&mut payload, candidate);
    write_network_address(&mut payload, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
    payload.extend_from_slice(&nonce);
    payload.push(user_agent_len);
    payload.extend_from_slice(user_agent.as_bytes());
    payload.extend_from_slice(&0_i32.to_le_bytes());
    payload.push(0);
    bitcoin_message("version", &payload, MAINNET_MAGIC)
}

fn write_network_address(output: &mut Vec<u8>, address: SocketAddr) {
    output.extend_from_slice(&0_u64.to_le_bytes());
    match address.ip() {
        IpAddr::V4(ip) => {
            output.extend_from_slice(&[0_u8; 10]);
            output.extend_from_slice(&[0xff, 0xff]);
            output.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => output.extend_from_slice(&ip.octets()),
    }
    output.extend_from_slice(&address.port().to_be_bytes());
}

fn bitcoin_message(command: &str, payload: &[u8], magic: [u8; 4]) -> Result<Vec<u8>> {
    if command.is_empty()
        || command.len() > 12
        || !command.bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("invalid Bitcoin P2P command {command:?}");
    }
    let payload_len = u32::try_from(payload.len()).context("Bitcoin P2P payload is too large")?;
    let mut result = Vec::with_capacity(MESSAGE_HEADER_BYTES + payload.len());
    result.extend_from_slice(&magic);
    let mut command_bytes = [0_u8; 12];
    command_bytes[..command.len()].copy_from_slice(command.as_bytes());
    result.extend_from_slice(&command_bytes);
    result.extend_from_slice(&payload_len.to_le_bytes());
    result.extend_from_slice(&payload_checksum(payload));
    result.extend_from_slice(payload);
    Ok(result)
}

struct P2pMessage {
    command: String,
    payload: Vec<u8>,
}

async fn read_message(stream: &mut TcpStream) -> Result<P2pMessage> {
    let mut header = [0_u8; MESSAGE_HEADER_BYTES];
    stream
        .read_exact(&mut header)
        .await
        .context("read message header")?;
    if header[..4] != MAINNET_MAGIC {
        bail!("response used non-mainnet network magic");
    }

    let command_field = &header[4..16];
    let command_end = command_field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(12);
    if command_end == 0
        || !command_field[..command_end]
            .iter()
            .all(u8::is_ascii_graphic)
        || command_field[command_end..].iter().any(|byte| *byte != 0)
    {
        bail!("response contained an invalid command field");
    }
    let command = std::str::from_utf8(&header[4..4 + command_end])
        .context("response command was not UTF-8")?
        .to_owned();
    let payload_len_bytes: [u8; 4] = header[16..20]
        .try_into()
        .context("read response payload length")?;
    let payload_len = usize::try_from(u32::from_le_bytes(payload_len_bytes))
        .context("convert response payload length")?;
    if payload_len > MAX_HANDSHAKE_PAYLOAD_BYTES {
        bail!(
            "response payload length {payload_len} exceeded the {MAX_HANDSHAKE_PAYLOAD_BYTES}-byte handshake limit"
        );
    }
    let checksum: [u8; 4] = header[20..24]
        .try_into()
        .context("read response checksum")?;
    let mut payload = vec![0_u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .context("read message payload")?;
    if checksum != payload_checksum(&payload) {
        bail!("response checksum mismatch for {command}");
    }
    Ok(P2pMessage { command, payload })
}

fn payload_checksum(payload: &[u8]) -> [u8; 4] {
    let first = digest(&SHA256, payload);
    let second = digest(&SHA256, first.as_ref());
    let mut checksum = [0_u8; 4];
    // `ring`'s SHA-256 implementation always returns a 32-byte digest.
    checksum.copy_from_slice(&second.as_ref()[..4]);
    checksum
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        process::{Child, Command, Stdio},
    };

    use super::*;
    use tokio::net::TcpListener;

    const TEST_TIMEOUTS: ProbeTimeouts = ProbeTimeouts {
        connect: Duration::from_millis(100),
        handshake: Duration::from_millis(100),
    };

    fn valid_version_payload() -> Vec<u8> {
        let mut payload = vec![0_u8; 80];
        payload[..4].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        payload
    }

    async fn spawn_valid_server(
        bind: &str,
    ) -> Result<(SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
        let listener = TcpListener::bind(bind).await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let version = read_message(&mut stream).await?;
            if version.command != "version" {
                bail!("probe did not send version first");
            }
            stream
                .write_all(&bitcoin_message(
                    "version",
                    &valid_version_payload(),
                    MAINNET_MAGIC,
                )?)
                .await?;
            let verack = read_message(&mut stream).await?;
            if verack.command != "verack" || !verack.payload.is_empty() {
                bail!("probe sent a malformed verack");
            }
            stream
                .write_all(&bitcoin_message("verack", &[], MAINNET_MAGIC)?)
                .await?;
            Result::<()>::Ok(())
        });
        Ok((endpoint, server))
    }

    #[test]
    fn version_message_serializes_current_fields_and_ipv4_network_order() -> Result<()> {
        let endpoint = SocketAddr::from(([127, 0, 0, 1], 8333));
        let message = version_message(endpoint)?;
        let payload = message
            .get(MESSAGE_HEADER_BYTES..)
            .context("test version payload")?;
        let expected_user_agent = format!("/BitEngine:{}/", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            i32::from_le_bytes(payload[0..4].try_into()?),
            PROTOCOL_VERSION
        );
        assert_eq!(u64::from_le_bytes(payload[4..12].try_into()?), 0);
        assert!(i64::from_le_bytes(payload[12..20].try_into()?) > 0);
        assert_eq!(
            &payload[28..44],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1]
        );
        assert_eq!(u16::from_be_bytes(payload[44..46].try_into()?), 8333);
        let user_agent_len = usize::from(payload[80]);
        assert_eq!(
            payload.get(81..81 + user_agent_len),
            Some(expected_user_agent.as_bytes())
        );
        let declared_checksum: [u8; 4] = message
            .get(20..24)
            .context("test version message checksum")?
            .try_into()?;
        assert_eq!(declared_checksum, payload_checksum(payload));
        Ok(())
    }

    #[test]
    fn version_message_serializes_ipv6_without_ipv4_mapping() -> Result<()> {
        let endpoint: SocketAddr = "[2001:db8::1]:18444".parse()?;
        let message = version_message(endpoint)?;
        let payload = message
            .get(MESSAGE_HEADER_BYTES..)
            .context("test version payload")?;
        let IpAddr::V6(ip) = endpoint.ip() else {
            bail!("test endpoint was not IPv6");
        };
        assert_eq!(&payload[28..44], &ip.octets());
        assert_eq!(u16::from_be_bytes(payload[44..46].try_into()?), 18_444);
        Ok(())
    }

    #[tokio::test]
    async fn handles_fragmented_and_coalesced_messages_with_intervening_commands() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let version = read_message(&mut stream).await?;
            if version.command != "version" {
                bail!("managed probe sent a malformed version message");
            }
            validate_version(&version.payload)?;

            let response = bitcoin_message("version", &valid_version_payload(), MAINNET_MAGIC)?;
            stream.write_all(&response[..7]).await?;
            tokio::task::yield_now().await;
            stream.write_all(&response[7..39]).await?;
            tokio::task::yield_now().await;
            stream.write_all(&response[39..]).await?;
            let verack = read_message(&mut stream).await?;
            if verack.command != "verack" || !verack.payload.is_empty() {
                bail!("managed probe sent a malformed verack message");
            }

            let mut coalesced = bitcoin_message("wtxidrelay", &[], MAINNET_MAGIC)?;
            coalesced.extend(bitcoin_message("sendaddrv2", &[], MAINNET_MAGIC)?);
            coalesced.extend(bitcoin_message("verack", &[], MAINNET_MAGIC)?);
            stream.write_all(&coalesced).await?;
            Result::<()>::Ok(())
        });

        assert_eq!(probe_p2p_candidates(&[endpoint]).await?, endpoint);
        server.await.context("join fragmented handshake server")??;
        Ok(())
    }

    #[tokio::test]
    async fn accepts_verack_received_before_version_without_early_success() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut response = bitcoin_message("verack", &[], MAINNET_MAGIC)?;
            response.extend(bitcoin_message(
                "version",
                &valid_version_payload(),
                MAINNET_MAGIC,
            )?);
            stream.write_all(&response).await?;
            let verack = read_message(&mut stream).await?;
            if verack.command != "verack" {
                bail!("probe did not finish its side of the handshake");
            }
            Result::<()>::Ok(())
        });

        assert_eq!(probe_p2p_candidates(&[endpoint]).await?, endpoint);
        server.await.context("join reordered handshake server")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_wrong_mainnet_magic_with_per_endpoint_diagnostics() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            stream
                .write_all(&bitcoin_message(
                    "version",
                    &valid_version_payload(),
                    [0x0b, 0x11, 0x09, 0x07],
                )?)
                .await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[endpoint]).await.unwrap_err();
        let diagnostic = error.to_string();
        assert!(diagnostic.contains(&endpoint.to_string()), "{error:#}");
        assert!(
            diagnostic.contains("non-mainnet network magic"),
            "{error:#}"
        );
        server.await.context("join non-mainnet server")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_bitcoin_message_with_an_invalid_checksum() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut response = bitcoin_message("version", &valid_version_payload(), MAINNET_MAGIC)?;
            response[20] ^= 0xff;
            stream.write_all(&response).await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[endpoint]).await.unwrap_err();
        assert!(error.to_string().contains("checksum"), "{error:#}");
        server.await.context("join invalid-checksum server")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_malformed_oversized_payload_length_without_allocating_it() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut header = bitcoin_message("version", &[], MAINNET_MAGIC)?;
            header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
            stream.write_all(&header).await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[endpoint]).await.unwrap_err();
        assert!(error.to_string().contains("payload length"), "{error:#}");
        server.await.context("join malformed-length server")??;
        Ok(())
    }

    #[tokio::test]
    async fn reports_peer_disconnect_before_version() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[endpoint]).await.unwrap_err();
        assert!(
            error.to_string().contains("peer closed before version"),
            "{error:#}"
        );
        server.await.context("join disconnecting server")??;
        Ok(())
    }

    #[tokio::test]
    async fn reports_timeout_waiting_for_verack() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            stream
                .write_all(&bitcoin_message(
                    "version",
                    &valid_version_payload(),
                    MAINNET_MAGIC,
                )?)
                .await?;
            let _ = read_message(&mut stream).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Result::<()>::Ok(())
        });
        let timeouts = ProbeTimeouts {
            handshake: Duration::from_millis(20),
            ..TEST_TIMEOUTS
        };

        let error = probe_p2p_candidates_with_timeouts(&[endpoint], timeouts)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("timed out waiting for verack"),
            "{error:#}"
        );
        server.await.context("join timeout server")??;
        Ok(())
    }

    #[tokio::test]
    async fn supports_ipv4_and_ipv6_candidates() -> Result<()> {
        let (ipv4, ipv4_server) = spawn_valid_server("127.0.0.1:0").await?;
        assert_eq!(probe_p2p_candidates(&[ipv4]).await?, ipv4);
        ipv4_server.await.context("join IPv4 server")??;

        let (ipv6, ipv6_server) = spawn_valid_server("[::1]:0").await?;
        assert_eq!(probe_p2p_candidates(&[ipv6]).await?, ipv6);
        ipv6_server.await.context("join IPv6 server")??;
        Ok(())
    }

    #[tokio::test]
    async fn selects_a_valid_fallback_after_a_refused_candidate() -> Result<()> {
        let refused_listener = TcpListener::bind("127.0.0.1:0").await?;
        let refused = refused_listener.local_addr()?;
        drop(refused_listener);
        let (valid, server) = spawn_valid_server("127.0.0.1:0").await?;

        assert_eq!(
            probe_p2p_candidates_with_timeouts(&[refused, valid], TEST_TIMEOUTS).await?,
            valid
        );
        server.await.context("join fallback server")??;
        Ok(())
    }

    #[cfg(unix)]
    struct CoreProcess(Child);

    #[cfg(unix)]
    impl Drop for CoreProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    #[ignore = "requires BITENGINE_LIVE_P2P_ENDPOINTS and probes an already-running mainnet Core"]
    async fn live_configured_core_candidates_complete_mainnet_handshake() -> Result<()> {
        let configured = std::env::var("BITENGINE_LIVE_P2P_ENDPOINTS")
            .context("set BITENGINE_LIVE_P2P_ENDPOINTS to comma-separated socket addresses")?;
        let candidates = configured
            .split(',')
            .map(str::trim)
            .map(str::parse)
            .collect::<std::result::Result<Vec<SocketAddr>, _>>()?;

        let selected = probe_p2p_candidates(&candidates).await?;
        assert!(candidates.contains(&selected));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires BITENGINE_BITCOIND and launches real Bitcoin Core with a temporary mainnet datadir"]
    async fn real_bitcoin_core_completes_mainnet_handshake_in_temporary_datadir() -> Result<()> {
        let bitcoind = std::env::var_os("BITENGINE_BITCOIND")
            .map(PathBuf::from)
            .context("set BITENGINE_BITCOIND to the supported bitcoind executable")?;
        let metadata = std::fs::symlink_metadata(&bitcoind)
            .with_context(|| format!("inspect {}", bitcoind.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("BITENGINE_BITCOIND must name a regular, non-symlink file");
        }

        let temporary = tempfile::tempdir()?;
        std::fs::write(temporary.path().join("bitcoin.conf"), "")?;
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
        let endpoint = reservation.local_addr()?;
        drop(reservation);
        let args = [
            format!("-datadir={}", temporary.path().display()),
            "-chain=main".to_owned(),
            "-listen=1".to_owned(),
            format!("-bind={endpoint}"),
            format!("-port={}", endpoint.port()),
            "-connect=0".to_owned(),
            "-dnsseed=0".to_owned(),
            "-fixedseeds=0".to_owned(),
            "-discover=0".to_owned(),
            "-listenonion=0".to_owned(),
            "-server=0".to_owned(),
            "-daemon=0".to_owned(),
            "-printtoconsole=0".to_owned(),
        ];
        let child = Command::new(&bitcoind)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("launch {}", bitcoind.display()))?;
        let mut process = CoreProcess(child);

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = process.0.try_wait()? {
                bail!("Bitcoin Core exited before opening P2P listener: {status}");
            }
            if probe_p2p_candidates_with_timeouts(&[endpoint], TEST_TIMEOUTS)
                .await
                .is_ok()
            {
                break;
            }
            if Instant::now() >= deadline {
                bail!("Bitcoin Core did not complete a mainnet handshake at {endpoint}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let process_id = libc::pid_t::try_from(process.0.id()).context("convert Core PID")?;
        // SAFETY: the PID belongs to the child process spawned immediately above.
        if unsafe { libc::kill(process_id, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error()).context("stop test Bitcoin Core");
        }
        let stop_deadline = Instant::now() + Duration::from_secs(10);
        while process.0.try_wait()?.is_none() {
            if Instant::now() >= stop_deadline {
                bail!("test Bitcoin Core did not stop after SIGTERM");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}
