//! Protocol-level health checks for the managed Bitcoin P2P service.

use std::{net::SocketAddr, time::Duration};

use anyhow::{bail, Context as _, Result};
use ring::digest::{digest, SHA256};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time::timeout,
};

const MAINNET_MAGIC: [u8; 4] = [0xf9, 0xbe, 0xb4, 0xd9];
const MAX_HANDSHAKE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_HANDSHAKE_MESSAGES: usize = 16;
const CANDIDATE_TIMEOUT: Duration = Duration::from_millis(750);
const TOTAL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

// A fixed, valid Bitcoin protocol version message. Its payload checksum is
// precomputed. The peer response supplies the current version and timestamp;
// the mainnet magic, checksums, version/verack exchange, and minimum protocol
// are validated.
const VERSION_MESSAGE: &[u8] = &[
    0xf9, 0xbe, 0xb4, 0xd9, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x67, 0x00, 0x00, 0x00, 0x2e, 0x4a, 0x54, 0xa6, 0x80, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, 0x11, 0x2f, 0x42, 0x69, 0x74, 0x45, 0x6e, 0x67,
    0x69, 0x6e, 0x65, 0x3a, 0x30, 0x2e, 0x31, 0x2e, 0x32, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const VERACK_MESSAGE: &[u8] = &[
    0xf9, 0xbe, 0xb4, 0xd9, 0x76, 0x65, 0x72, 0x61, 0x63, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x5d, 0xf6, 0xe0, 0xe2,
];

/// Return the first configured endpoint that completes a bounded mainnet
/// `version`/`verack` exchange in deterministic candidate order.
pub async fn probe_p2p_candidates(candidates: &[SocketAddr]) -> Result<SocketAddr> {
    timeout(TOTAL_PROBE_TIMEOUT, async {
        let mut failures = Vec::with_capacity(candidates.len());
        for &candidate in candidates {
            match timeout(CANDIDATE_TIMEOUT, probe_p2p(candidate)).await {
                Ok(Ok(())) => return Ok(candidate),
                Ok(Err(error)) => failures.push(format!("{candidate}: {error}")),
                Err(_) => failures.push(format!("{candidate}: handshake timed out")),
            }
        }
        bail!(
            "no configured Bitcoin P2P endpoint completed a mainnet handshake ({})",
            failures.join("; ")
        )
    })
    .await
    .context("Bitcoin P2P endpoint probe timed out")?
}

async fn probe_p2p(candidate: SocketAddr) -> Result<()> {
    let mut stream = TcpStream::connect(candidate)
        .await
        .with_context(|| format!("connect to Bitcoin P2P endpoint {candidate}"))?;
    stream
        .write_all(VERSION_MESSAGE)
        .await
        .context("send Bitcoin P2P version")?;

    let mut saw_version = false;
    for _ in 0..MAX_HANDSHAKE_MESSAGES {
        let message = read_message(&mut stream).await?;
        match message.command.as_str() {
            "version" => {
                if message.payload.len() < 80 {
                    bail!("Bitcoin P2P version payload was too short");
                }
                let version_bytes: [u8; 4] = message.payload[..4]
                    .try_into()
                    .context("read Bitcoin P2P protocol version")?;
                let version = i32::from_le_bytes(version_bytes);
                if version < 70_001 {
                    bail!("Bitcoin P2P protocol version {version} is unsupported");
                }
                stream
                    .write_all(VERACK_MESSAGE)
                    .await
                    .context("send Bitcoin P2P verack")?;
                saw_version = true;
            }
            "verack" if saw_version => {
                if !message.payload.is_empty() {
                    bail!("Bitcoin P2P verack was malformed");
                }
                return Ok(());
            }
            _ => {}
        }
    }

    bail!("Bitcoin P2P peer did not complete version/verack")
}

struct P2pMessage {
    command: String,
    payload: Vec<u8>,
}

async fn read_message(stream: &mut TcpStream) -> Result<P2pMessage> {
    let mut header = [0_u8; 24];
    stream
        .read_exact(&mut header)
        .await
        .context("read Bitcoin P2P message header")?;
    if header[..4] != MAINNET_MAGIC {
        bail!("Bitcoin P2P peer returned non-mainnet network magic");
    }

    let command_end = header[4..16]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(12);
    let command = std::str::from_utf8(&header[4..4 + command_end])
        .context("Bitcoin P2P command was not UTF-8")?
        .to_owned();
    let payload_len_bytes: [u8; 4] = header[16..20]
        .try_into()
        .context("read Bitcoin P2P payload length")?;
    let payload_len = usize::try_from(u32::from_le_bytes(payload_len_bytes))
        .context("convert Bitcoin P2P payload length")?;
    if payload_len > MAX_HANDSHAKE_PAYLOAD_BYTES {
        bail!("Bitcoin P2P handshake payload exceeded the size limit");
    }
    let checksum: [u8; 4] = header[20..24]
        .try_into()
        .context("read Bitcoin P2P checksum")?;
    let mut payload = vec![0_u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .context("read Bitcoin P2P message payload")?;
    if checksum != payload_checksum(&payload) {
        bail!("Bitcoin P2P message checksum was invalid");
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
    use super::*;
    use tokio::net::TcpListener;

    fn message(command: &str, payload: &[u8], magic: [u8; 4]) -> Vec<u8> {
        let mut result = Vec::with_capacity(24 + payload.len());
        result.extend_from_slice(&magic);
        let mut command_bytes = [0_u8; 12];
        command_bytes[..command.len()].copy_from_slice(command.as_bytes());
        result.extend_from_slice(&command_bytes);
        result.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or_default()
                .to_le_bytes(),
        );
        result.extend_from_slice(&payload_checksum(payload));
        result.extend_from_slice(payload);
        result
    }

    #[tokio::test]
    async fn accepts_a_complete_mainnet_version_handshake() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let version = read_message(&mut stream).await?;
            if version.command != "version" || version.payload.len() != 103 {
                anyhow::bail!("managed probe sent a malformed version message");
            }
            let mut payload = vec![0_u8; 80];
            payload[..4].copy_from_slice(&70_016_i32.to_le_bytes());
            stream
                .write_all(&message("version", &payload, MAINNET_MAGIC))
                .await?;
            let verack = read_message(&mut stream).await?;
            if verack.command != "verack" || !verack.payload.is_empty() {
                anyhow::bail!("managed probe sent a malformed verack message");
            }
            stream
                .write_all(&message("verack", &[], MAINNET_MAGIC))
                .await?;
            Result::<()>::Ok(())
        });

        assert_eq!(probe_p2p_candidates(&[addr]).await?, addr);
        server.await.context("join mock Bitcoin P2P server")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_listener_that_is_not_mainnet_bitcoin() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let version = read_message(&mut stream).await?;
            if version.command != "version" || version.payload.len() != 103 {
                anyhow::bail!("managed probe sent a malformed version message");
            }
            let mut payload = vec![0_u8; 80];
            payload[..4].copy_from_slice(&70_016_i32.to_le_bytes());
            stream
                .write_all(&message("version", &payload, [0x0b, 0x11, 0x09, 0x07]))
                .await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[addr]).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("no configured Bitcoin P2P endpoint"));
        server.await.context("join mock non-mainnet server")??;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_bitcoin_message_with_an_invalid_checksum() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut payload = vec![0_u8; 80];
            payload[..4].copy_from_slice(&70_016_i32.to_le_bytes());
            let mut response = message("version", &payload, MAINNET_MAGIC);
            response[20] ^= 0xff;
            stream.write_all(&response).await?;
            Result::<()>::Ok(())
        });

        let error = probe_p2p_candidates(&[addr]).await.unwrap_err();
        assert!(error.to_string().contains("checksum"), "{error:#}");
        server.await.context("join invalid-checksum server")??;
        Ok(())
    }

    #[tokio::test]
    async fn selects_the_first_candidate_that_completes_a_mainnet_handshake() -> Result<()> {
        let wrong_listener = TcpListener::bind("127.0.0.1:0").await?;
        let wrong_addr = wrong_listener.local_addr()?;
        let wrong_server = tokio::spawn(async move {
            let (mut stream, _) = wrong_listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut payload = vec![0_u8; 80];
            payload[..4].copy_from_slice(&70_016_i32.to_le_bytes());
            stream
                .write_all(&message("version", &payload, [0x0b, 0x11, 0x09, 0x07]))
                .await?;
            Result::<()>::Ok(())
        });

        let valid_listener = TcpListener::bind("127.0.0.1:0").await?;
        let valid_addr = valid_listener.local_addr()?;
        let valid_server = tokio::spawn(async move {
            let (mut stream, _) = valid_listener.accept().await?;
            let _ = read_message(&mut stream).await?;
            let mut payload = vec![0_u8; 80];
            payload[..4].copy_from_slice(&70_016_i32.to_le_bytes());
            stream
                .write_all(&message("version", &payload, MAINNET_MAGIC))
                .await?;
            let _ = read_message(&mut stream).await?;
            stream
                .write_all(&message("verack", &[], MAINNET_MAGIC))
                .await?;
            Result::<()>::Ok(())
        });

        assert_eq!(
            probe_p2p_candidates(&[wrong_addr, valid_addr]).await?,
            valid_addr
        );
        wrong_server.await.context("join wrong-network server")??;
        valid_server.await.context("join valid fallback server")??;
        Ok(())
    }
}
