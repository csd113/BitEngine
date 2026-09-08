//! Wallet-connection readiness and endpoint construction.
//!
//! This module interprets facts produced by the existing Bitcoin/electrs
//! lifecycle. It does not own or poll either service, which keeps one source of
//! truth for process management while giving the dashboard a fail-closed model
//! for deciding whether connection details are usable.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU16,
    time::Duration,
};

use thiserror::Error;
use tokio::time::timeout;

use crate::{electrs_status::ElectrsStatus, rpc::BlockchainInfo};

pub const DEFAULT_ELECTRUM_PORT: u16 = 50_001;
const LAN_DISCOVERY_ROUTE: &str = "192.0.2.1:9";
const LAN_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);

/// The intentional exposure policy for the managed electrs listener.
///
/// `Default` is deliberately loopback-only. LAN exposure must always be named
/// at the process launch call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ElectrsBindPolicy {
    #[default]
    LoopbackOnly,
    LocalNetwork,
}

impl From<bool> for ElectrsBindPolicy {
    fn from(local_network_access: bool) -> Self {
        if local_network_access {
            Self::LocalNetwork
        } else {
            Self::LoopbackOnly
        }
    }
}

impl ElectrsBindPolicy {
    #[must_use]
    pub(crate) const fn requires_lan_address(self) -> bool {
        matches!(self, Self::LocalNetwork)
    }
}

/// The concrete, validated listener passed to one managed electrs generation.
/// A LAN policy cannot be represented without naming one private interface IP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElectrsListenAddr {
    policy: ElectrsBindPolicy,
    socket: SocketAddr,
}

impl ElectrsListenAddr {
    /// Resolve configured intent into the exact address electrs will bind.
    ///
    /// # Errors
    ///
    /// LAN access requires a discovered RFC1918 or IPv6 ULA address. Wildcard,
    /// public, link-local, loopback, and multicast addresses fail closed.
    pub(crate) fn for_policy(
        policy: ElectrsBindPolicy,
        lan_ip: Option<IpAddr>,
        port: u16,
    ) -> Result<Self, EndpointError> {
        let port = nonzero_port(port)?;
        let ip = match policy {
            ElectrsBindPolicy::LoopbackOnly => IpAddr::V4(Ipv4Addr::LOCALHOST),
            ElectrsBindPolicy::LocalNetwork => {
                let ip = lan_ip.ok_or(EndpointError::MissingLanAddress)?;
                validate_lan_ip(ip)?;
                ip
            }
        };
        Ok(Self {
            policy,
            socket: SocketAddr::from((ip, port.get())),
        })
    }

    #[must_use]
    pub(crate) const fn policy(self) -> ElectrsBindPolicy {
        self.policy
    }

    #[must_use]
    pub(crate) const fn socket_addr(self) -> SocketAddr {
        self.socket
    }
}

/// Service facts from the current managed Bitcoin generation.
#[derive(Debug, Clone, Copy)]
pub struct BitcoinReadiness<'a> {
    pub(crate) process_running: bool,
    pub(crate) blockchain_info: Option<&'a BlockchainInfo>,
    /// Fatal process, compatibility, or RPC diagnostic from the lifecycle.
    pub(crate) error: Option<&'a str>,
    /// P2P readiness can fail briefly while a running Core process warms up.
    pub(crate) p2p_error: Option<&'a str>,
}

/// Service facts from the current managed electrs generation.
#[derive(Debug, Clone, Copy)]
pub struct ElectrsReadiness<'a> {
    pub(crate) status: &'a ElectrsStatus,
    /// Unexpected child-exit or launch diagnostic retained by the lifecycle.
    pub(crate) process_error: Option<&'a str>,
}

/// Why a wallet endpoint is, or is not, currently usable.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionReadiness {
    ServicesStopped,
    BitcoinStarting,
    BitcoinFailed {
        reason: String,
    },
    BitcoinSyncing {
        percent: f64,
        blocks: u64,
        headers: u64,
    },
    ElectrsStopped,
    ElectrsStarting,
    ElectrsIndexing {
        percent: Option<f64>,
        indexed_height: Option<u64>,
        bitcoin_height: Option<u64>,
    },
    ElectrsUnavailable {
        reason: String,
    },
    Ready,
}

impl ConnectionReadiness {
    /// Interpret the latest nonblocking status-poll results.
    #[must_use]
    pub(crate) fn evaluate(bitcoin: BitcoinReadiness<'_>, electrs: ElectrsReadiness<'_>) -> Self {
        if let Some(error) = nonempty(bitcoin.error) {
            return Self::BitcoinFailed {
                reason: error.to_owned(),
            };
        }
        if !bitcoin.process_running {
            return Self::ServicesStopped;
        }

        let Some(blockchain_info) = bitcoin.blockchain_info else {
            return Self::BitcoinStarting;
        };
        if !bitcoin_is_synced(blockchain_info) {
            return Self::BitcoinSyncing {
                percent: bitcoin_sync_percent(blockchain_info),
                blocks: blockchain_info.blocks,
                headers: blockchain_info.headers,
            };
        }
        if let Some(error) = nonempty(bitcoin.p2p_error) {
            return Self::BitcoinFailed {
                reason: error.to_owned(),
            };
        }

        if let Some(error) = nonempty(electrs.process_error) {
            return Self::ElectrsUnavailable {
                reason: error.to_owned(),
            };
        }
        if !electrs.status.running {
            return Self::ElectrsStopped;
        }
        if electrs.status.is_connection_ready() {
            return Self::Ready;
        }

        if let Some(reason) = electrs_error(electrs.status) {
            return Self::ElectrsUnavailable {
                reason: reason.to_owned(),
            };
        }

        if electrs_is_indexing(electrs.status) {
            return Self::ElectrsIndexing {
                percent: electrs
                    .status
                    .sync_percent
                    .map(normalize_incomplete_percent),
                indexed_height: electrs.status.electrs_height,
                bitcoin_height: electrs.status.bitcoin_blocks,
            };
        }

        Self::ElectrsStarting
    }

    #[must_use]
    pub(crate) const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Human-readable explanation for the always-visible Connect card.
    #[must_use]
    pub(crate) fn message(&self) -> String {
        match self {
            Self::ServicesStopped => "Node services are stopped. Start Bitcoin Core to begin."
                .to_owned(),
            Self::BitcoinStarting => {
                "Bitcoin Core is starting. Waiting for blockchain synchronization status…"
                    .to_owned()
            }
            Self::BitcoinFailed { reason } => format!("Bitcoin Core is unavailable — {reason}"),
            Self::BitcoinSyncing { percent, .. } => {
                format!("Bitcoin Core is still syncing — {percent:.1}% complete")
            }
            Self::ElectrsStopped => {
                "Bitcoin Core is synced. Start electrs to make wallet connections available."
                    .to_owned()
            }
            Self::ElectrsStarting => {
                "Bitcoin Core is synced. Waiting for electrs to become ready…".to_owned()
            }
            Self::ElectrsIndexing { percent, .. } => percent.map_or_else(
                || "Bitcoin Core is synced. electrs is indexing the blockchain…".to_owned(),
                |percent| {
                    format!(
                        "Bitcoin Core is synced. electrs is indexing the blockchain — {percent:.1}% complete"
                    )
                },
            ),
            Self::ElectrsUnavailable { reason } => {
                format!("electrs is temporarily unavailable — {reason}")
            }
            Self::Ready => "Bitcoin Core and electrs are synced. Your node is ready."
                .to_owned(),
        }
    }
}

const fn bitcoin_is_synced(info: &BlockchainInfo) -> bool {
    !info.initial_block_download && info.blocks >= info.headers
}

fn bitcoin_sync_percent(info: &BlockchainInfo) -> f64 {
    normalize_incomplete_percent(info.verification_progress * 100.0)
}

const fn normalize_incomplete_percent(percent: f64) -> f64 {
    normalize_percent(percent).min(99.9)
}

const fn normalize_percent(percent: f64) -> f64 {
    if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn electrs_is_indexing(status: &ElectrsStatus) -> bool {
    status.electrs_height.is_some()
        || status.sync_percent.is_some()
        || status
            .connect_error
            .as_deref()
            .is_some_and(is_index_not_ready)
}

fn electrs_error(status: &ElectrsStatus) -> Option<&str> {
    [
        status.bitcoin_error.as_deref(),
        status
            .connect_error
            .as_deref()
            .filter(|error| !is_index_not_ready(error)),
        status.metrics_error.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|error| !error.trim().is_empty())
}

fn is_index_not_ready(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("index is not ready") || lower.contains("unavailable index")
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.trim().is_empty())
}

/// Whether an endpoint is intended for this computer, its LAN, or Tor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    SameMachine,
    LocalNetwork,
    Tor,
}

/// A validated Electrum TCP endpoint that cannot contain credentials, paths,
/// queries, cookies, or unrelated application state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectrumEndpoint {
    kind: EndpointKind,
    host: EndpointHost,
    port: NonZeroU16,
}

impl ElectrumEndpoint {
    /// Construct the loopback endpoint intended only for a wallet on this host.
    pub(crate) fn same_machine(port: u16) -> Result<Self, EndpointError> {
        Ok(Self {
            kind: EndpointKind::SameMachine,
            host: EndpointHost::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            port: nonzero_port(port)?,
        })
    }

    /// Construct an endpoint advertised to another device on the LAN.
    ///
    /// # Errors
    ///
    /// Rejects addresses that cannot name a remote LAN client target, including
    /// wildcard, loopback, multicast, and scope-dependent IPv6 link-local IPs.
    pub(crate) fn local_network(ip: IpAddr, port: u16) -> Result<Self, EndpointError> {
        validate_lan_ip(ip)?;
        Ok(Self {
            kind: EndpointKind::LocalNetwork,
            host: EndpointHost::Ip(ip),
            port: nonzero_port(port)?,
        })
    }

    /// Construct a plaintext Electrum endpoint transported through Tor.
    ///
    /// # Errors
    ///
    /// Rejects anything other than a canonical lowercase v3 onion hostname.
    pub(crate) fn onion(host: &str, port: u16) -> Result<Self, EndpointError> {
        validate_v3_onion(host)?;
        Ok(Self {
            kind: EndpointKind::Tor,
            host: EndpointHost::Name(host.to_owned()),
            port: nonzero_port(port)?,
        })
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn kind(&self) -> EndpointKind {
        self.kind
    }

    #[must_use]
    pub(crate) fn host(&self) -> String {
        self.host.to_string()
    }

    #[must_use]
    pub(crate) const fn port(&self) -> u16 {
        self.port.get()
    }

    /// Host and port exactly as shown beside the QR code.
    #[must_use]
    pub(crate) fn authority(&self) -> String {
        match self.host {
            EndpointHost::Ip(IpAddr::V6(ip)) => format!("[{ip}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    /// Canonical QR and clipboard payload accepted by compatible wallets.
    #[must_use]
    pub(crate) fn payload(&self) -> String {
        format!("tcp://{}", self.authority())
    }

    #[must_use]
    pub(crate) const fn protocol_label(&self) -> &'static str {
        match self.kind {
            EndpointKind::SameMachine | EndpointKind::LocalNetwork => "Electrum TCP",
            EndpointKind::Tor => "Electrum TCP over Tor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointHost {
    Ip(IpAddr),
    Name(String),
}

impl fmt::Display for EndpointHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(ip) => ip.fmt(formatter),
            Self::Name(name) => formatter.write_str(name),
        }
    }
}

/// Effective local endpoint state. Configured intent and the listener that is
/// actually running are kept separate so a toggle never claims exposure that
/// has not taken effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEndpointState {
    ServiceNotRunning,
    RestartRequired {
        configured: ElectrsBindPolicy,
        active: ElectrsBindPolicy,
    },
    SameMachineOnly {
        endpoint: ElectrumEndpoint,
    },
    AddressUnavailable {
        reason: String,
    },
    Available {
        endpoint: ElectrumEndpoint,
    },
}

impl LocalEndpointState {
    /// Resolve the endpoint that is genuinely reachable under the effective
    /// electrs bind. The active listener is already a validated snapshot of the
    /// exact interface used by this process generation.
    #[must_use]
    pub(crate) fn resolve(
        configured: ElectrsBindPolicy,
        active: Option<ElectrsListenAddr>,
    ) -> Self {
        let Some(active) = active else {
            return Self::ServiceNotRunning;
        };
        if configured != active.policy() {
            return Self::RestartRequired {
                configured,
                active: active.policy(),
            };
        }

        match active.policy() {
            ElectrsBindPolicy::LoopbackOnly => {
                match ElectrumEndpoint::same_machine(active.socket_addr().port()) {
                    Ok(endpoint) => Self::SameMachineOnly { endpoint },
                    Err(error) => Self::AddressUnavailable {
                        reason: error.to_string(),
                    },
                }
            }
            ElectrsBindPolicy::LocalNetwork => {
                match ElectrumEndpoint::local_network(
                    active.socket_addr().ip(),
                    active.socket_addr().port(),
                ) {
                    Ok(endpoint) => Self::Available { endpoint },
                    Err(error) => Self::AddressUnavailable {
                        reason: error.to_string(),
                    },
                }
            }
        }
    }

    #[must_use]
    pub(crate) const fn endpoint(&self) -> Option<&ElectrumEndpoint> {
        match self {
            Self::SameMachineOnly { endpoint } | Self::Available { endpoint } => Some(endpoint),
            Self::ServiceNotRunning
            | Self::RestartRequired { .. }
            | Self::AddressUnavailable { .. } => None,
        }
    }

    #[must_use]
    pub(crate) const fn is_lan_reachable(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    #[must_use]
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            Self::ServiceNotRunning => Some(
                "electrs is not running, so no local endpoint is listening.".to_owned(),
            ),
            Self::RestartRequired { configured, .. } => Some(match configured {
                ElectrsBindPolicy::LoopbackOnly => {
                    "Restart electrs to disable local-network access.".to_owned()
                }
                ElectrsBindPolicy::LocalNetwork => {
                    "Restart electrs to enable local-network access.".to_owned()
                }
            }),
            Self::SameMachineOnly { .. } => Some(
                "Local network access is disabled. This endpoint works only on this computer."
                    .to_owned(),
            ),
            Self::AddressUnavailable { reason } => Some(format!(
                "Local network access is enabled, but BitEngine could not determine a reachable address — {reason}"
            )),
            Self::Available { .. } => None,
        }
    }
}

/// Determine the source address chosen by the operating system's IPv4 default
/// route without sending application data. Run this from an Iced `Task`; all
/// socket work is asynchronous and bounded.
pub async fn discover_lan_address() -> Result<IpAddr, EndpointError> {
    timeout(LAN_DISCOVERY_TIMEOUT, async {
        let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(EndpointError::LanDiscovery)?;
        socket
            .connect(LAN_DISCOVERY_ROUTE)
            .await
            .map_err(EndpointError::LanDiscovery)?;
        let ip = socket
            .local_addr()
            .map_err(EndpointError::LanDiscovery)?
            .ip();
        validate_lan_ip(ip)?;
        Ok(ip)
    })
    .await
    .map_err(|_| EndpointError::LanDiscoveryTimeout)?
}

/// Reconfirm that an active listener still belongs to this host's current
/// private default-route interface.
///
/// Loopback listeners need no route discovery. A LAN listener fails closed on
/// discovery failure or address change; callers must keep that failure sticky
/// for the lifetime of the managed electrs generation.
pub async fn revalidate_active_listener(listener: ElectrsListenAddr) -> Result<(), EndpointError> {
    if listener.policy() == ElectrsBindPolicy::LoopbackOnly {
        return Ok(());
    }
    validate_listener_discovery(listener, discover_lan_address().await)
}

fn validate_listener_discovery(
    listener: ElectrsListenAddr,
    discovered: Result<IpAddr, EndpointError>,
) -> Result<(), EndpointError> {
    if listener.policy() == ElectrsBindPolicy::LoopbackOnly {
        return Ok(());
    }
    let discovered = discovered?;
    let active = listener.socket_addr().ip();
    if discovered != active {
        return Err(EndpointError::LanAddressChanged { active, discovered });
    }
    Ok(())
}

const fn validate_lan_ip(ip: IpAddr) -> Result<(), EndpointError> {
    let is_private_lan = match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    };
    if !is_private_lan {
        return Err(EndpointError::UnreachableLanAddress(ip));
    }
    Ok(())
}

fn validate_v3_onion(host: &str) -> Result<(), EndpointError> {
    let Some(service_id) = host.strip_suffix(".onion") else {
        return Err(EndpointError::InvalidOnionHostname);
    };
    if service_id.len() != 56
        || !service_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
    {
        return Err(EndpointError::InvalidOnionHostname);
    }
    host.parse::<tor_hsservice::HsId>()
        .map_err(|_| EndpointError::InvalidOnionHostname)?;
    Ok(())
}

fn nonzero_port(port: u16) -> Result<NonZeroU16, EndpointError> {
    NonZeroU16::new(port).ok_or(EndpointError::ZeroPort)
}

#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("electrum endpoint port must not be zero")]
    ZeroPort,
    #[error("local-network access requires a validated private interface address")]
    MissingLanAddress,
    #[error("local-network address is not remotely reachable: {0}")]
    UnreachableLanAddress(IpAddr),
    #[error("onion endpoint is not a canonical v3 hostname")]
    InvalidOnionHostname,
    #[error("could not determine the local-network address: {0}")]
    LanDiscovery(#[source] std::io::Error),
    #[error("timed out while determining the local-network address")]
    LanDiscoveryTimeout,
    #[error("active local-network address changed from {active} to {discovered}")]
    LanAddressChanged { active: IpAddr, discovered: IpAddr },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blockchain_info(
        blocks: u64,
        headers: u64,
        verification_progress: f64,
        initial_block_download: bool,
    ) -> BlockchainInfo {
        BlockchainInfo {
            blocks,
            headers,
            verification_progress,
            initial_block_download,
            pruned: false,
        }
    }

    fn electrs_status(indexed: u64, bitcoin: u64, ready: bool) -> ElectrsStatus {
        let indexed_percent = u32::try_from(indexed)
            .ok()
            .zip(u32::try_from(bitcoin).ok())
            .map(|(indexed, bitcoin)| (f64::from(indexed) / f64::from(bitcoin) * 100.0).min(100.0));
        ElectrsStatus {
            running: true,
            connected: true,
            synced: indexed >= bitcoin,
            ready,
            electrs_height: Some(indexed),
            bitcoin_blocks: Some(bitcoin),
            bitcoin_headers: Some(bitcoin),
            sync_percent: indexed_percent,
            ..ElectrsStatus::default()
        }
    }

    fn readiness(
        info: Option<&BlockchainInfo>,
        bitcoin_error: Option<&str>,
        electrs: &ElectrsStatus,
    ) -> ConnectionReadiness {
        ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: true,
                blockchain_info: info,
                error: bitcoin_error,
                p2p_error: None,
            },
            ElectrsReadiness {
                status: electrs,
                process_error: None,
            },
        )
    }

    #[test]
    fn bitcoin_initial_download_gates_connection_with_real_progress() {
        let bitcoin = blockchain_info(800_000, 850_000, 0.734, true);
        let electrs = electrs_status(850_000, 850_000, true);

        let state = readiness(Some(&bitcoin), None, &electrs);

        assert_eq!(
            state,
            ConnectionReadiness::BitcoinSyncing {
                percent: 73.4,
                blocks: 800_000,
                headers: 850_000,
            }
        );
        assert_eq!(
            state.message(),
            "Bitcoin Core is still syncing — 73.4% complete"
        );
        assert!(!state.is_ready());
    }

    #[test]
    fn matching_heights_do_not_override_bitcoin_initial_download() {
        let bitcoin = blockchain_info(850_000, 850_000, 0.999, true);
        let electrs = electrs_status(850_000, 850_000, true);

        assert!(matches!(
            readiness(Some(&bitcoin), None, &electrs),
            ConnectionReadiness::BitcoinSyncing { .. }
        ));
    }

    #[test]
    fn incomplete_sync_progress_never_claims_one_hundred_percent() {
        let bitcoin = blockchain_info(849_999, 850_000, 0.999_999, false);
        let electrs = electrs_status(849_999, 850_000, true);

        let bitcoin_state = readiness(Some(&bitcoin), None, &electrs);
        assert!(matches!(
            bitcoin_state,
            ConnectionReadiness::BitcoinSyncing { percent: 99.9, .. }
        ));
        assert!(bitcoin_state.message().contains("99.9% complete"));

        let synced_bitcoin = blockchain_info(850_000, 850_000, 1.0, false);
        let electrs_state = readiness(Some(&synced_bitcoin), None, &electrs);
        assert!(matches!(
            electrs_state,
            ConnectionReadiness::ElectrsIndexing {
                percent: Some(99.9),
                ..
            }
        ));
        assert!(electrs_state.message().contains("99.9% complete"));
    }

    #[test]
    fn p2p_warmup_errors_do_not_override_startup_or_sync_progress() {
        let electrs = ElectrsStatus::default();
        let p2p_error = "no configured P2P endpoint completed a handshake: connection refused";

        let starting = ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: true,
                blockchain_info: None,
                error: None,
                p2p_error: Some(p2p_error),
            },
            ElectrsReadiness {
                status: &electrs,
                process_error: None,
            },
        );
        assert_eq!(starting, ConnectionReadiness::BitcoinStarting);

        let syncing_info = blockchain_info(849_999, 850_000, 0.999_999, false);
        let syncing = ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: true,
                blockchain_info: Some(&syncing_info),
                error: None,
                p2p_error: Some(p2p_error),
            },
            ElectrsReadiness {
                status: &electrs,
                process_error: None,
            },
        );
        assert!(matches!(
            syncing,
            ConnectionReadiness::BitcoinSyncing { .. }
        ));

        let synced_info = blockchain_info(850_000, 850_000, 1.0, false);
        let unavailable = ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: true,
                blockchain_info: Some(&synced_info),
                error: None,
                p2p_error: Some(p2p_error),
            },
            ElectrsReadiness {
                status: &electrs,
                process_error: None,
            },
        );
        assert!(matches!(
            unavailable,
            ConnectionReadiness::BitcoinFailed { .. }
        ));
    }

    #[test]
    fn electrs_protocol_readiness_without_index_sync_is_gated() {
        let bitcoin = blockchain_info(850_000, 850_000, 1.0, false);
        let electrs = electrs_status(765_000, 850_000, true);

        let state = readiness(Some(&bitcoin), None, &electrs);

        assert!(matches!(
            state,
            ConnectionReadiness::ElectrsIndexing {
                indexed_height: Some(765_000),
                bitcoin_height: Some(850_000),
                ..
            }
        ));
        assert!(!state.is_ready());
        assert!(state.message().contains("electrs is indexing"));
    }

    #[test]
    fn electrs_dependency_error_is_not_hidden_by_stale_index_progress() {
        let bitcoin = blockchain_info(850_000, 850_000, 1.0, false);
        let mut electrs = electrs_status(765_000, 850_000, true);
        electrs.bitcoin_error = Some("managed Bitcoin RPC generation is unavailable".to_owned());

        let state = readiness(Some(&bitcoin), None, &electrs);

        assert!(matches!(
            state,
            ConnectionReadiness::ElectrsUnavailable { .. }
        ));
        assert!(state.message().contains("managed Bitcoin RPC"));
    }

    #[test]
    fn transition_from_syncing_to_ready_requires_both_services() {
        let syncing_bitcoin = blockchain_info(849_000, 850_000, 0.98, true);
        let synced_bitcoin = blockchain_info(850_000, 850_000, 1.0, false);
        let indexing = electrs_status(849_500, 850_000, true);
        let ready = electrs_status(850_000, 850_000, true);

        assert!(matches!(
            readiness(Some(&syncing_bitcoin), None, &indexing),
            ConnectionReadiness::BitcoinSyncing { .. }
        ));
        assert!(matches!(
            readiness(Some(&synced_bitcoin), None, &indexing),
            ConnectionReadiness::ElectrsIndexing { .. }
        ));
        assert_eq!(
            readiness(Some(&synced_bitcoin), None, &ready),
            ConnectionReadiness::Ready
        );
    }

    #[test]
    fn unavailable_qr_always_has_a_visible_reason() {
        let electrs = ElectrsStatus::default();
        for state in [
            ConnectionReadiness::ServicesStopped,
            ConnectionReadiness::BitcoinStarting,
            ConnectionReadiness::BitcoinFailed {
                reason: "managed process exited; see the Bitcoin log".to_owned(),
            },
            ConnectionReadiness::ElectrsStopped,
            ConnectionReadiness::ElectrsStarting,
            ConnectionReadiness::ElectrsUnavailable {
                reason: "connection refused".to_owned(),
            },
            readiness(None, None, &electrs),
        ] {
            assert!(!state.is_ready());
            assert!(!state.message().trim().is_empty());
        }
    }

    #[test]
    fn failures_are_actionable_and_take_precedence_over_stopped_state() {
        let electrs = ElectrsStatus::default();
        let bitcoin = ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: false,
                blockchain_info: None,
                error: Some("process exited with status 1; see the Bitcoin log"),
                p2p_error: None,
            },
            ElectrsReadiness {
                status: &electrs,
                process_error: None,
            },
        );
        assert!(matches!(bitcoin, ConnectionReadiness::BitcoinFailed { .. }));

        let synced = blockchain_info(850_000, 850_000, 1.0, false);
        let electrs_failure = ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: true,
                blockchain_info: Some(&synced),
                error: None,
                p2p_error: None,
            },
            ElectrsReadiness {
                status: &electrs,
                process_error: Some("process exited with status 2; see the electrs log"),
            },
        );
        assert!(matches!(
            electrs_failure,
            ConnectionReadiness::ElectrsUnavailable { .. }
        ));
    }

    #[test]
    fn bind_policy_is_loopback_by_default_and_lan_requires_a_private_ip(
    ) -> Result<(), EndpointError> {
        assert_eq!(
            ElectrsListenAddr::for_policy(
                ElectrsBindPolicy::default(),
                None,
                DEFAULT_ELECTRUM_PORT,
            )?
            .socket_addr(),
            "127.0.0.1:50001".parse().expect("static socket address")
        );
        assert_eq!(
            ElectrsListenAddr::for_policy(
                ElectrsBindPolicy::LocalNetwork,
                Some("192.168.1.42".parse().expect("static IP")),
                DEFAULT_ELECTRUM_PORT,
            )?
            .socket_addr(),
            "192.168.1.42:50001".parse().expect("static socket address")
        );
        assert!(ElectrsListenAddr::for_policy(ElectrsBindPolicy::LoopbackOnly, None, 0).is_err());
        assert!(ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            None,
            DEFAULT_ELECTRUM_PORT,
        )
        .is_err());
        for public_or_wildcard in ["0.0.0.0", "169.254.1.2", "8.8.8.8", "2001:4860:4860::8888"] {
            assert!(ElectrsListenAddr::for_policy(
                ElectrsBindPolicy::LocalNetwork,
                Some(public_or_wildcard.parse().expect("static IP")),
                DEFAULT_ELECTRUM_PORT,
            )
            .is_err());
        }
        Ok(())
    }

    #[test]
    fn active_lan_listener_revalidation_requires_the_same_discovered_ip(
    ) -> Result<(), EndpointError> {
        let listener = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            Some("192.168.1.42".parse().expect("static IP")),
            DEFAULT_ELECTRUM_PORT,
        )?;

        assert!(validate_listener_discovery(
            listener,
            Ok("192.168.1.42".parse().expect("static IP"))
        )
        .is_ok());
        assert!(matches!(
            validate_listener_discovery(listener, Ok("192.168.1.99".parse().expect("static IP"))),
            Err(EndpointError::LanAddressChanged { .. })
        ));
        assert!(matches!(
            validate_listener_discovery(listener, Err(EndpointError::LanDiscoveryTimeout)),
            Err(EndpointError::LanDiscoveryTimeout)
        ));
        Ok(())
    }

    #[test]
    fn loopback_revalidation_never_depends_on_lan_discovery() -> Result<(), EndpointError> {
        let listener = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LoopbackOnly,
            None,
            DEFAULT_ELECTRUM_PORT,
        )?;
        assert!(
            validate_listener_discovery(listener, Err(EndpointError::LanDiscoveryTimeout)).is_ok()
        );
        Ok(())
    }

    #[test]
    fn disabled_lan_access_advertises_same_machine_only() {
        let state = LocalEndpointState::resolve(
            ElectrsBindPolicy::LoopbackOnly,
            Some(
                ElectrsListenAddr::for_policy(
                    ElectrsBindPolicy::LoopbackOnly,
                    None,
                    DEFAULT_ELECTRUM_PORT,
                )
                .expect("loopback listener"),
            ),
        );

        let endpoint = state.endpoint().expect("same-machine endpoint");
        assert_eq!(endpoint.kind(), EndpointKind::SameMachine);
        assert_eq!(endpoint.authority(), "127.0.0.1:50001");
        assert!(!state.is_lan_reachable());
        assert!(state
            .message()
            .is_some_and(|message| message.contains("only on this computer")));
    }

    #[test]
    fn configured_lan_access_is_not_advertised_until_effective() {
        let pending = LocalEndpointState::resolve(
            ElectrsBindPolicy::LocalNetwork,
            Some(
                ElectrsListenAddr::for_policy(
                    ElectrsBindPolicy::LoopbackOnly,
                    None,
                    DEFAULT_ELECTRUM_PORT,
                )
                .expect("loopback listener"),
            ),
        );
        assert!(matches!(
            pending,
            LocalEndpointState::RestartRequired { .. }
        ));
        assert!(pending.endpoint().is_none());

        let stopped = LocalEndpointState::resolve(ElectrsBindPolicy::LocalNetwork, None);
        assert_eq!(stopped, LocalEndpointState::ServiceNotRunning);
        assert!(stopped.endpoint().is_none());
    }

    #[test]
    fn active_lan_endpoint_matches_display_copy_and_qr_payload() {
        let state = LocalEndpointState::resolve(
            ElectrsBindPolicy::LocalNetwork,
            Some(
                ElectrsListenAddr::for_policy(
                    ElectrsBindPolicy::LocalNetwork,
                    Some("192.168.1.42".parse().expect("static IP")),
                    DEFAULT_ELECTRUM_PORT,
                )
                .expect("LAN listener"),
            ),
        );

        let endpoint = state.endpoint().expect("LAN endpoint");
        assert!(state.is_lan_reachable());
        assert_eq!(endpoint.host(), "192.168.1.42");
        assert_eq!(endpoint.port(), DEFAULT_ELECTRUM_PORT);
        assert_eq!(endpoint.authority(), "192.168.1.42:50001");
        assert_eq!(endpoint.payload(), "tcp://192.168.1.42:50001");
        assert_eq!(endpoint.protocol_label(), "Electrum TCP");
    }

    #[test]
    fn ipv6_payload_uses_brackets_and_same_authority_as_display() -> Result<(), EndpointError> {
        let endpoint =
            ElectrumEndpoint::local_network("fd00::42".parse().expect("static IP"), 50001)?;
        assert_eq!(endpoint.authority(), "[fd00::42]:50001");
        assert_eq!(endpoint.payload(), "tcp://[fd00::42]:50001");
        Ok(())
    }

    #[test]
    fn onion_payload_is_canonical_and_contains_no_rpc_secrets() -> Result<(), EndpointError> {
        // Valid v3 test vector from C Tor's hidden-service address tests.
        let host = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";
        let endpoint = ElectrumEndpoint::onion(host, DEFAULT_ELECTRUM_PORT)?;
        let payload = endpoint.payload();

        assert_eq!(endpoint.kind(), EndpointKind::Tor);
        assert_eq!(endpoint.authority(), format!("{host}:50001"));
        assert_eq!(payload, format!("tcp://{host}:50001"));
        assert_eq!(endpoint.protocol_label(), "Electrum TCP over Tor");
        for secret_marker in ["rpcuser", "rpcpassword", ".cookie", "@", "?", "#"] {
            assert!(!payload.contains(secret_marker));
        }
        Ok(())
    }

    #[test]
    fn malformed_or_secret_bearing_onion_hosts_are_rejected() {
        for host in [
            "short.onion",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.onion",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1.onion",
            "user:password@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion/path",
        ] {
            assert!(ElectrumEndpoint::onion(host, DEFAULT_ELECTRUM_PORT).is_err());
        }
    }

    #[test]
    fn wildcard_loopback_multicast_and_scoped_ipv6_are_not_lan_endpoints() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "::",
            "::1",
            "ff02::1",
            "fe80::1",
        ] {
            let ip = ip.parse().expect("static IP");
            assert!(ElectrumEndpoint::local_network(ip, DEFAULT_ELECTRUM_PORT).is_err());
        }
    }
}
