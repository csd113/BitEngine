use std::time::Duration;

use anyhow::{Context as _, Result};
use reqwest::StatusCode;
use tokio::{net::TcpStream, time::timeout};

use crate::{
    config::Config,
    rpc::{self, BlockchainInfo, RpcAuth},
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ElectrsStatus {
    pub running: bool,
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

pub async fn probe(config: &Config, process_running: bool) -> ElectrsStatus {
    if !process_running {
        return ElectrsStatus::default();
    }

    let auth = RpcAuth::from_data_dir(&config.bitcoin_data_path);
    let metrics_url = Config::electrs_metrics_url();
    let electrum_addr = Config::electrum_addr().to_owned();

    let (metrics_result, bitcoin_result, connect_result) = tokio::join!(
        fetch_metrics(&metrics_url),
        rpc::get_blockchain_info(&auth),
        check_connectivity(&electrum_addr),
    );

    build_status(
        process_running,
        metrics_result.map_err(|e| e.to_string()),
        bitcoin_result.map_err(|e| e.to_string()),
        connect_result.map_err(|e| e.to_string()),
    )
}

async fn fetch_metrics(url: &str) -> Result<String> {
    let response = reqwest::Client::builder()
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

    response.text().await.context("read electrs metrics body")
}

fn build_status(
    process_running: bool,
    metrics_result: Result<String, String>,
    bitcoin_result: Result<BlockchainInfo, String>,
    connect_result: Result<(), String>,
) -> ElectrsStatus {
    let running = process_running || metrics_result.is_ok();

    let (electrs_height, metrics_error) = match metrics_result {
        Ok(metrics) => match parse_tip_height(&metrics) {
            Ok(height) => (Some(height), None),
            Err(err) => (None, Some(err)),
        },
        Err(err) => (None, Some(format!("metrics unreachable: {err}"))),
    };

    let (bitcoin_blocks, bitcoin_headers, bitcoin_error) = match bitcoin_result {
        Ok(info) => (Some(info.blocks), Some(info.headers), None),
        Err(err) => (None, None, Some(format!("Bitcoin Core unavailable: {err}"))),
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

    let synced = matches!(
        (electrs_height, bitcoin_blocks),
        (Some(index_height), Some(blocks)) if index_height >= blocks
    ) && metrics_error.is_none()
        && bitcoin_error.is_none();

    let connect_error = connect_result
        .err()
        .map(|err| format!("connect failed: {err}"));

    // "Ready" follows the UI label semantics: a client is ready to connect when
    // the Electrum port accepts a real TCP connection, even if indexing is still
    // in progress. This stays separate from the "Synced" light on purpose.
    let ready = connect_error.is_none();

    ElectrsStatus {
        running,
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

    let Some(raw_value) = line.split_whitespace().last() else {
        return Err("electrs tip height metric malformed".to_owned());
    };

    raw_value
        .parse::<u64>()
        .map_err(|_| "electrs tip height metric malformed".to_owned())
}

async fn check_connectivity(addr: &str) -> Result<()> {
    timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .context("electrum TCP connect timed out")?
        .context("electrum TCP connect failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{build_status, parse_tip_height, probe, ElectrsStatus};
    use crate::{config::Config, rpc::BlockchainInfo};

    #[test]
    fn parses_tip_height_metric() {
        let metrics = "# HELP electrs_index_height tip height\n\
electrs_index_height{type=\"tip\"} 856201\n";

        assert_eq!(parse_tip_height(metrics), Ok(856_201));
    }

    #[test]
    fn reports_synced_when_heights_match() {
        let status = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Ok(BlockchainInfo {
                blocks: 856_201,
                headers: 856_201,
                verification_progress: 1.0,
            }),
            Err("refused".to_owned()),
        );

        assert!(status.synced);
        assert_eq!(status.sync_percent, Some(100.0));
    }

    #[test]
    fn reports_not_synced_when_electrs_lags() {
        let status = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} 856100".to_owned()),
            Ok(BlockchainInfo {
                blocks: 856_201,
                headers: 856_205,
                verification_progress: 1.0,
            }),
            Err("refused".to_owned()),
        );

        assert!(!status.synced);
        assert_eq!(status.electrs_height, Some(856_100));
        assert_eq!(status.bitcoin_blocks, Some(856_201));
        assert!(status.sync_percent.is_some_and(|percent| percent < 100.0));
    }

    #[test]
    fn handles_missing_or_malformed_metrics_without_panicking() {
        let missing = build_status(
            false,
            Ok("# no electrs tip metric here".to_owned()),
            Ok(BlockchainInfo {
                blocks: 100,
                headers: 100,
                verification_progress: 1.0,
            }),
            Err("refused".to_owned()),
        );
        assert!(!missing.synced);
        assert!(missing.metrics_error.is_some());

        let malformed = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} nope".to_owned()),
            Ok(BlockchainInfo {
                blocks: 100,
                headers: 100,
                verification_progress: 1.0,
            }),
            Err("refused".to_owned()),
        );
        assert!(!malformed.synced);
        assert!(malformed.metrics_error.is_some());

        let unreachable = build_status(
            false,
            Err("connection reset".to_owned()),
            Ok(BlockchainInfo {
                blocks: 100,
                headers: 100,
                verification_progress: 1.0,
            }),
            Err("refused".to_owned()),
        );
        assert!(!unreachable.synced);
        assert!(unreachable.metrics_error.is_some());
    }

    #[test]
    fn handles_bitcoin_unavailable_without_panicking() {
        let status = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Err("rpc auth failed".to_owned()),
            Err("refused".to_owned()),
        );

        assert!(!status.synced);
        assert!(status.bitcoin_error.is_some());
        assert_eq!(status.sync_percent, None);
    }

    #[tokio::test]
    async fn tcp_connect_success_marks_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let status = build_status(
            false,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            super::check_connectivity(&addr.to_string())
                .await
                .map_err(|e| e.to_string()),
        );

        assert!(status.ready);
        accept_task.await.expect("accept task join");
    }

    #[tokio::test]
    async fn tcp_connect_refused_marks_not_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");
        drop(listener);

        let status = build_status(
            false,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            super::check_connectivity(&addr.to_string())
                .await
                .map_err(|e| e.to_string()),
        );

        assert!(!status.ready);
        assert!(status.connect_error.is_some());
    }

    #[test]
    fn running_is_true_from_process_check_even_if_metrics_fail() {
        let status = build_status(
            true,
            Err("metrics down".to_owned()),
            Err("bitcoin down".to_owned()),
            Err("refused".to_owned()),
        );

        assert!(status.running);
    }

    #[test]
    fn running_is_true_from_metrics_even_without_process_check() {
        let status = build_status(
            false,
            Ok("electrs_index_height{type=\"tip\"} 856201".to_owned()),
            Err("bitcoin down".to_owned()),
            Err("refused".to_owned()),
        );

        assert!(status.running);
    }

    #[tokio::test]
    async fn probe_is_quiet_when_electrs_process_is_not_running() {
        let config = Config {
            binaries_path: PathBuf::from("/missing/binaries"),
            bitcoin_data_path: PathBuf::from("/missing/bitcoin"),
            electrs_data_path: PathBuf::from("/missing/electrs"),
        };

        let status = probe(&config, false).await;

        assert_eq!(status, ElectrsStatus::default());
    }
}
