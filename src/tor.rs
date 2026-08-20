//! Embedded Arti onion-service lifecycle for the managed Electrum endpoint.
//!
//! This module deliberately has no general-purpose port-forwarding API.  Its
//! only exposure plan is the fixed Electrum virtual port forwarded to a
//! validated local address on the same fixed port.  In particular, Bitcoin
//! Core RPC, metrics, and `BitEngine` UI/admin endpoints are not representable.

#![expect(
    clippy::redundant_pub_crate,
    reason = "crate-only visibility is intentional even while the root tor module remains private"
)]

use std::{
    future::Future,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use arti_client::config::TorClientConfigBuilder;
use arti_client::{BootstrapBehavior, DormantMode, TorClient, TorClientConfig};
use fs_mistrust::Mistrust;
use futures::{stream::FuturesUnordered, Stream, StreamExt as _};
use safelog::DisplayRedacted as _;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tor_cell::relaycell::msg::{Connected, End, EndReason};
use tor_hsservice::{
    config::OnionServiceConfigBuilder,
    status::{OnionServiceStatusStream, State as OnionServiceState},
    HsId, HsNickname, RendRequest, RunningOnionService, StreamRequest,
};
use tor_proto::stream::IncomingStreamRequest;
use tor_rtcompat::{NetStreamProvider as _, PreferredRuntime};

/// The sole virtual and target port exposed by `BitEngine`'s onion service.
pub const ELECTRUM_ONION_PORT: u16 = crate::connection::DEFAULT_ELECTRUM_PORT;

const SERVICE_NICKNAME: &str = "bitengine-electrs";
const STORAGE_DIRECTORY_NAME: &str = "tor";
const COMMAND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 32;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_mins(1);
const ONION_SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const ATTEMPT_STOP_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_STORAGE_ENTRIES: usize = 100_000;
const MAX_STORAGE_DEPTH: usize = 64;
const MAX_DIAGNOSTIC_CHARS: usize = 600;

/// A validated socket belonging to `BitEngine`'s managed Electrum listener.
///
/// Construction is crate-private so external/user input cannot turn the Tor
/// backend into a generic proxy.  The listener may be loopback, or an exact
/// RFC1918/IPv6-ULA interface when the separate LAN setting deliberately
/// enables that same managed Electrum service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElectrsTorTarget(SocketAddr);

impl ElectrsTorTarget {
    /// Validate an application-selected managed Electrum listener.
    pub(crate) const fn new(address: SocketAddr) -> Result<Self, TorConfigError> {
        if address.port() != ELECTRUM_ONION_PORT {
            return Err(TorConfigError::UnexpectedTargetPort {
                port: address.port(),
            });
        }
        if !is_private_local_address(address.ip()) {
            return Err(TorConfigError::UnsafeTargetAddress { address });
        }
        Ok(Self(address))
    }

    const fn socket_addr(self) -> SocketAddr {
        self.0
    }
}

/// One atomic desired reverse-proxy snapshot.
///
/// Keeping readiness and target together prevents a listener change from being
/// observed with readiness that belonged to the previous listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElectrsProxyState {
    target: ElectrsTorTarget,
    ready: bool,
    revision: u64,
}

impl ElectrsProxyState {
    const fn new(target: ElectrsTorTarget, ready: bool, revision: u64) -> Self {
        Self {
            target,
            ready,
            revision,
        }
    }
}

const fn is_private_local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback() || is_unique_local_v6(address),
    }
}

const fn is_unique_local_v6(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xfe00 == 0xfc00
}

/// Validated, immutable inputs for one embedded Tor supervisor.
#[derive(Debug, Clone)]
pub(crate) struct TorManagerConfig {
    storage_root: PathBuf,
    target: ElectrsTorTarget,
    initially_enabled: bool,
    initially_electrs_ready: bool,
}

impl TorManagerConfig {
    /// Construct configuration with an explicitly supplied trusted storage root.
    ///
    /// Production integration should prefer [`Self::for_bitengine_config`];
    /// this constructor also supports isolated temporary directories in tests.
    pub(crate) fn new(
        storage_root: PathBuf,
        target: ElectrsTorTarget,
    ) -> Result<Self, TorConfigError> {
        validate_storage_root_lexically(&storage_root)?;
        Ok(Self {
            storage_root,
            target,
            initially_enabled: false,
            initially_electrs_ready: false,
        })
    }

    /// Derive `BitEngine`-owned Tor storage beside the application's config file.
    ///
    /// For the normal platform config path this yields
    /// `<BitEngine config directory>/tor/{state,cache}`. The Tor path is not a
    /// persisted/user-configurable setting.
    pub(crate) fn for_bitengine_config(
        config_file: &Path,
        target: ElectrsTorTarget,
    ) -> Result<Self, TorConfigError> {
        let parent = config_file
            .parent()
            .ok_or_else(|| TorConfigError::InvalidStorageRoot {
                problem: "BitEngine config path has no parent directory".to_owned(),
            })?;
        Self::new(parent.join(STORAGE_DIRECTORY_NAME), target)
    }

    /// Set whether the supervisor should start Arti immediately.
    #[must_use]
    pub(crate) const fn initially_enabled(mut self, enabled: bool) -> Self {
        self.initially_enabled = enabled;
        self
    }

    /// Set the initial readiness of the managed electrs service.
    #[must_use]
    pub(crate) const fn initially_electrs_ready(mut self, ready: bool) -> Self {
        self.initially_electrs_ready = ready;
        self
    }

    #[cfg(test)]
    fn storage_root(&self) -> &Path {
        &self.storage_root
    }
}

/// Failures detected before a Tor worker can safely start.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum TorConfigError {
    /// The selected target is not a local/private managed listener.
    #[error(
        "Tor may only target an exact loopback or private managed electrs address, not {address}"
    )]
    UnsafeTargetAddress { address: SocketAddr },
    /// The selected target is not the fixed Electrum port.
    #[error("Tor may only expose the managed electrs port {ELECTRUM_ONION_PORT}, not {port}")]
    UnexpectedTargetPort { port: u16 },
    /// The private storage root is lexically unsafe.
    #[error("invalid Tor storage root: {problem}")]
    InvalidStorageRoot { problem: String },
    /// The manager was started without an active Tokio runtime.
    #[error("embedded Tor must be started from the asynchronous application runtime")]
    RuntimeUnavailable,
}

/// The latest user-facing state of the embedded Tor service.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum TorStatus {
    /// Tor is disabled.  Existing identity material remains on disk.
    Disabled,
    /// Private storage and the embedded Arti client are being initialized.
    Starting,
    /// Arti is acquiring enough Tor directory state to operate safely.
    Bootstrapping {
        progress: u8,
        summary: Option<String>,
    },
    /// Introduction points and the v3 onion descriptor are being published.
    Publishing { onion_host: String },
    /// Tor is published, but electrs is not presently ready to answer wallets.
    WaitingForElectrs { onion_host: String },
    /// The onion descriptor is reachable and electrs is ready.
    Available { onion_host: String },
    /// A recoverable Tor/network/service interruption is being handled.
    TemporarilyUnavailable {
        message: String,
        onion_host: Option<String>,
        retry_in: Option<Duration>,
    },
    /// Tor cannot safely start without correction or an explicit retry.
    Error { message: String, retryable: bool },
}

impl TorStatus {
    /// Return the complete public v3 hostname when one is known.
    #[must_use]
    pub(crate) fn onion_host(&self) -> Option<&str> {
        match self {
            Self::Publishing { onion_host }
            | Self::WaitingForElectrs { onion_host }
            | Self::Available { onion_host }
            | Self::TemporarilyUnavailable {
                onion_host: Some(onion_host),
                ..
            } => Some(onion_host),
            Self::Disabled
            | Self::Starting
            | Self::Bootstrapping { .. }
            | Self::TemporarilyUnavailable {
                onion_host: None, ..
            }
            | Self::Error { .. } => None,
        }
    }

    /// Whether the displayed endpoint is currently usable.
    #[must_use]
    pub(crate) const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// A cheap cloneable command/status handle for the embedded Tor supervisor.
///
/// Dropping a temporary clone does not stop Tor.  Use [`Self::shutdown`] for a
/// joined application shutdown; dropping the final clone initiates cooperative
/// cancellation without blocking the GUI thread.
#[derive(Debug, Clone)]
pub(crate) struct TorManager {
    inner: Arc<TorManagerInner>,
}

#[derive(Debug)]
struct TorManagerInner {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<TorStatus>,
    shutdown: CancellationToken,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for TorManagerInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(task) = self.task.get_mut() {
            // Detach the supervisor after signalling it. It owns the active
            // attempt and will cooperatively cancel it without blocking Drop.
            drop(task.take());
        }
    }
}

impl TorManager {
    /// Spawn the supervisor on the current Tokio runtime.
    ///
    /// In Iced this should be invoked from a `Task::perform` future so a Tokio
    /// runtime context is guaranteed.
    pub(crate) fn spawn(config: TorManagerConfig) -> Result<Self, TorConfigError> {
        tokio::runtime::Handle::try_current().map_err(|_| TorConfigError::RuntimeUnavailable)?;
        Self::spawn_with_factory(
            config,
            ArtiAttemptFactory::default(),
            RetryPolicy::production(),
        )
    }

    fn spawn_with_factory<F>(
        config: TorManagerConfig,
        factory: F,
        retry_policy: RetryPolicy,
    ) -> Result<Self, TorConfigError>
    where
        F: AttemptFactory,
    {
        tokio::runtime::Handle::try_current().map_err(|_| TorConfigError::RuntimeUnavailable)?;
        let initial_status = if config.initially_enabled {
            TorStatus::Starting
        } else {
            TorStatus::Disabled
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (status_tx, status_rx) = watch::channel(initial_status);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_supervisor(
            config,
            factory,
            retry_policy,
            command_rx,
            status_tx,
            shutdown.clone(),
        ));
        Ok(Self {
            inner: Arc::new(TorManagerInner {
                commands: command_tx,
                status: status_rx,
                shutdown,
                task: StdMutex::new(Some(task)),
            }),
        })
    }

    /// Return the latest status without waiting.
    #[must_use]
    pub(crate) fn status(&self) -> TorStatus {
        self.inner.status.borrow().clone()
    }

    /// Subscribe to coalesced latest-value status updates.
    #[must_use]
    pub(crate) fn subscribe(&self) -> watch::Receiver<TorStatus> {
        self.inner.status.clone()
    }

    /// Enable or disable onion publication asynchronously.
    ///
    /// Disabling stops onion publication, makes the shared client dormant, and
    /// never deletes identity/state. Reusing one client for the manager's
    /// lifetime avoids competing Arti instances retaining the same state locks.
    pub(crate) async fn set_enabled(&self, enabled: bool) -> Result<(), TorControlError> {
        self.send_acknowledged(|acknowledged| Command::SetEnabled {
            enabled,
            acknowledged,
        })
        .await
    }

    /// Atomically update the managed electrs listener and its readiness.
    ///
    /// A not-ready snapshot actively rejects new onion streams. While Tor is
    /// active, success means the reverse proxy applied the complete snapshot;
    /// reconfiguration never relaunches or rotates the onion identity.
    pub(crate) async fn set_electrs_state(
        &self,
        target: ElectrsTorTarget,
        ready: bool,
    ) -> Result<(), TorControlError> {
        self.send_acknowledged(|acknowledged| Command::SetElectrsState {
            target,
            ready,
            acknowledged,
        })
        .await
    }

    /// Cancel the current attempt/backoff and retry immediately with the same storage.
    pub(crate) async fn retry(&self) -> Result<(), TorControlError> {
        self.send_acknowledged(|acknowledged| Command::Retry { acknowledged })
            .await
    }

    /// Stop publication and wait for the supervisor to exit.
    pub(crate) async fn shutdown(&self) -> Result<(), TorControlError> {
        let command_result = self
            .send_acknowledged(|acknowledged| Command::Shutdown { acknowledged })
            .await;
        let task = {
            self.inner
                .task
                .lock()
                .map_err(|_| {
                    TorControlError::WorkerJoin("supervisor task lock was poisoned".to_owned())
                })?
                .take()
        };
        let join_result = match task {
            Some(task) => task.await.map_err(|error| {
                TorControlError::WorkerJoin(bounded_diagnostic(error.to_string()))
            }),
            None => Ok(()),
        };
        match join_result {
            Ok(()) => command_result,
            Err(error) => Err(error),
        }
    }

    async fn send_acknowledged(
        &self,
        command: impl FnOnce(oneshot::Sender<()>) -> Command,
    ) -> Result<(), TorControlError> {
        let (acknowledged, response) = oneshot::channel();
        self.inner
            .commands
            .send(command(acknowledged))
            .await
            .map_err(|_| TorControlError::WorkerStopped)?;
        response.await.map_err(|_| TorControlError::WorkerStopped)
    }
}

/// Errors sending a lifecycle command or joining the supervisor.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum TorControlError {
    /// The worker has already exited and cannot accept the command.
    #[error("the embedded Tor worker is no longer running")]
    WorkerStopped,
    /// The worker task panicked or was externally cancelled.
    #[error("the embedded Tor worker stopped unexpectedly: {0}")]
    WorkerJoin(String),
}

#[derive(Debug)]
enum Command {
    SetEnabled {
        enabled: bool,
        acknowledged: oneshot::Sender<()>,
    },
    SetElectrsState {
        target: ElectrsTorTarget,
        ready: bool,
        acknowledged: oneshot::Sender<()>,
    },
    Retry {
        acknowledged: oneshot::Sender<()>,
    },
    Shutdown {
        acknowledged: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    initial: Duration,
    maximum: Duration,
    stop_timeout: Duration,
}

impl RetryPolicy {
    const fn production() -> Self {
        Self {
            initial: INITIAL_RETRY_DELAY,
            maximum: MAX_RETRY_DELAY,
            stop_timeout: ATTEMPT_STOP_TIMEOUT,
        }
    }
}

trait AttemptFactory: Clone + Send + Sync + 'static {
    fn run(
        self,
        config: TorManagerConfig,
        events: mpsc::Sender<AttemptEvent>,
        proxy_state: watch::Receiver<ElectrsProxyState>,
        cancelled: CancellationToken,
    ) -> impl Future<Output = Result<(), AttemptFailure>> + Send + 'static;
}

/// Production attempts share one Arti client for the supervisor's lifetime.
///
/// Arti 0.45 does not expose a stable hard-shutdown operation for `TorClient`;
/// dropping its public handle can leave spawned manager tasks holding storage
/// locks. Keeping the client here and placing it in soft dormancy when the
/// onion service stops makes repeated toggles deterministic and prevents a
/// second client from competing for the same private state.
#[derive(Clone, Default)]
struct ArtiAttemptFactory {
    client: Arc<StdMutex<Option<Arc<TorClient<PreferredRuntime>>>>>,
}

impl ArtiAttemptFactory {
    fn cached_client(&self) -> Result<Option<Arc<TorClient<PreferredRuntime>>>, AttemptFailure> {
        self.client
            .lock()
            .map(|client| client.clone())
            .map_err(|_| AttemptFailure::fatal("embedded Tor client cache lock was poisoned"))
    }

    fn retain_client(
        &self,
        client: Arc<TorClient<PreferredRuntime>>,
    ) -> Result<Arc<TorClient<PreferredRuntime>>, AttemptFailure> {
        let mut cached = self
            .client
            .lock()
            .map_err(|_| AttemptFailure::fatal("embedded Tor client cache lock was poisoned"))?;
        Ok(Arc::clone(cached.get_or_insert(client)))
    }
}

impl AttemptFactory for ArtiAttemptFactory {
    fn run(
        self,
        config: TorManagerConfig,
        events: mpsc::Sender<AttemptEvent>,
        proxy_state: watch::Receiver<ElectrsProxyState>,
        cancelled: CancellationToken,
    ) -> impl Future<Output = Result<(), AttemptFailure>> + Send + 'static {
        run_arti_attempt(self, config, events, proxy_state, cancelled)
    }
}

#[derive(Debug, Clone)]
enum AttemptEvent {
    Bootstrapping {
        progress: u8,
        summary: Option<String>,
    },
    Identity(OnionHostname),
    Publishing,
    ProxyApplied {
        revision: u64,
        ready: bool,
    },
    Reachable,
    TemporarilyUnavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OnionHostname(String);

impl OnionHostname {
    fn from_hsid(hsid: HsId) -> Self {
        Self(hsid.display_unredacted().to_string())
    }

    #[cfg(test)]
    fn parse(hostname: &str) -> Result<Self, AttemptFailure> {
        let hsid = hostname.parse::<HsId>().map_err(|error| {
            AttemptFailure::fatal(format!("invalid test onion hostname: {error}"))
        })?;
        Ok(Self::from_hsid(hsid))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
enum AttemptPhase {
    Starting,
    Bootstrapping {
        progress: u8,
        summary: Option<String>,
    },
    Publishing,
    Reachable,
    TemporarilyUnavailable(String),
}

#[derive(Debug)]
struct SupervisorState {
    enabled: bool,
    electrs_ready: bool,
    proxy_applied_ready: bool,
    proxy_revision: u64,
    onion_host: Option<OnionHostname>,
    phase: AttemptPhase,
    retry_delay: Duration,
}

impl SupervisorState {
    const fn new(config: &TorManagerConfig, retry_policy: RetryPolicy) -> Self {
        Self {
            enabled: config.initially_enabled,
            electrs_ready: config.initially_electrs_ready,
            proxy_applied_ready: false,
            proxy_revision: 0,
            onion_host: None,
            phase: AttemptPhase::Starting,
            retry_delay: retry_policy.initial,
        }
    }

    fn status(&self) -> TorStatus {
        if !self.enabled {
            return TorStatus::Disabled;
        }
        match &self.phase {
            AttemptPhase::Starting => TorStatus::Starting,
            AttemptPhase::Bootstrapping { progress, summary } => TorStatus::Bootstrapping {
                progress: *progress,
                summary: summary.clone(),
            },
            AttemptPhase::Publishing => {
                self.onion_host
                    .as_ref()
                    .map_or(TorStatus::Starting, |host| TorStatus::Publishing {
                        onion_host: host.as_str().to_owned(),
                    })
            }
            AttemptPhase::Reachable => self.onion_host.as_ref().map_or_else(
                || TorStatus::Error {
                    message: "Arti reported reachability without an onion identity".to_owned(),
                    retryable: true,
                },
                |host| {
                    if self.electrs_ready && self.proxy_applied_ready {
                        TorStatus::Available {
                            onion_host: host.as_str().to_owned(),
                        }
                    } else {
                        TorStatus::WaitingForElectrs {
                            onion_host: host.as_str().to_owned(),
                        }
                    }
                },
            ),
            AttemptPhase::TemporarilyUnavailable(message) => TorStatus::TemporarilyUnavailable {
                message: message.clone(),
                onion_host: self
                    .onion_host
                    .as_ref()
                    .map(|hostname| hostname.as_str().to_owned()),
                retry_in: None,
            },
        }
    }

    fn apply(&mut self, event: AttemptEvent, retry_policy: RetryPolicy) {
        match event {
            AttemptEvent::Bootstrapping { progress, summary } => {
                self.phase = AttemptPhase::Bootstrapping { progress, summary };
            }
            AttemptEvent::Identity(hostname) => {
                self.onion_host = Some(hostname);
                self.phase = AttemptPhase::Publishing;
            }
            AttemptEvent::Publishing => self.phase = AttemptPhase::Publishing,
            AttemptEvent::ProxyApplied { revision, ready } => {
                if revision == self.proxy_revision {
                    self.proxy_applied_ready = ready;
                }
            }
            AttemptEvent::Reachable => {
                self.phase = AttemptPhase::Reachable;
                self.retry_delay = retry_policy.initial;
            }
            AttemptEvent::TemporarilyUnavailable(message) => {
                self.phase = AttemptPhase::TemporarilyUnavailable(message);
            }
        }
    }

    const fn invalidate_proxy_readiness(&mut self) {
        self.electrs_ready = false;
        self.proxy_applied_ready = false;
        self.proxy_revision = self.proxy_revision.wrapping_add(1);
    }
}

#[derive(Debug, Clone)]
struct AttemptFailure {
    message: String,
    retryable: bool,
    relaunch_safe: bool,
}

impl AttemptFailure {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: bounded_diagnostic(message.into()),
            retryable: false,
            relaunch_safe: true,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: bounded_diagnostic(message.into()),
            retryable: true,
            relaunch_safe: true,
        }
    }

    fn unconfirmed_teardown(message: impl Into<String>) -> Self {
        Self {
            message: bounded_diagnostic(message.into()),
            retryable: false,
            relaunch_safe: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveExitKind {
    Restart,
    Disabled,
    Shutdown,
}

#[derive(Debug)]
enum ActiveExit {
    Control(ActiveExitKind),
    Failed(AttemptFailure),
    UnsafeTeardown(AttemptFailure),
}

async fn run_supervisor<F>(
    mut config: TorManagerConfig,
    factory: F,
    retry_policy: RetryPolicy,
    mut commands: mpsc::Receiver<Command>,
    status: watch::Sender<TorStatus>,
    shutdown: CancellationToken,
) where
    F: AttemptFactory,
{
    let mut state = SupervisorState::new(&config, retry_policy);
    publish_status(&status, state.status());

    loop {
        if !state.enabled {
            match wait_while_disabled(&mut commands, &mut config, &mut state, &status, &shutdown)
                .await
            {
                ActiveExitKind::Restart => {}
                ActiveExitKind::Disabled => continue,
                ActiveExitKind::Shutdown => return,
            }
        }

        state.phase = AttemptPhase::Starting;
        state.proxy_applied_ready = false;
        publish_status(&status, state.status());
        match run_active_attempt(
            &mut config,
            factory.clone(),
            retry_policy,
            &mut commands,
            &mut state,
            &status,
            &shutdown,
        )
        .await
        {
            ActiveExit::Control(ActiveExitKind::Restart) => {
                state.invalidate_proxy_readiness();
            }
            ActiveExit::Control(ActiveExitKind::Disabled) => {
                state.invalidate_proxy_readiness();
                state.enabled = false;
                publish_status(&status, TorStatus::Disabled);
            }
            ActiveExit::Control(ActiveExitKind::Shutdown) => return,
            ActiveExit::UnsafeTeardown(failure) => {
                state.invalidate_proxy_readiness();
                publish_status(
                    &status,
                    TorStatus::Error {
                        message: failure.message,
                        retryable: false,
                    },
                );
                // The old Arti service may still own publisher/IPT-manager
                // state. Closing the supervisor makes every queued or future
                // re-enable fail until a process restart, so the same nickname
                // cannot overlap an unconfirmed teardown.
                return;
            }
            ActiveExit::Failed(failure) => {
                // Never carry readiness from a failed proxy instance into its
                // replacement. A fresh application snapshot must explicitly
                // enable forwarding again; the validated target is retained.
                state.invalidate_proxy_readiness();
                let retry = failure.retryable.then_some(state.retry_delay);
                if let Some(delay) = retry {
                    publish_status(
                        &status,
                        TorStatus::TemporarilyUnavailable {
                            message: failure.message,
                            onion_host: state
                                .onion_host
                                .as_ref()
                                .map(|host| host.as_str().to_owned()),
                            retry_in: Some(delay),
                        },
                    );
                    state.retry_delay = state
                        .retry_delay
                        .saturating_mul(2)
                        .min(retry_policy.maximum);
                } else {
                    publish_status(
                        &status,
                        TorStatus::Error {
                            message: failure.message,
                            retryable: false,
                        },
                    );
                }

                match wait_after_failure(
                    retry,
                    retry_policy,
                    &mut commands,
                    &mut config,
                    &mut state,
                    &status,
                    &shutdown,
                )
                .await
                {
                    ActiveExitKind::Restart => {}
                    ActiveExitKind::Disabled => {
                        state.enabled = false;
                        publish_status(&status, TorStatus::Disabled);
                    }
                    ActiveExitKind::Shutdown => return,
                }
            }
        }
    }
}

async fn wait_while_disabled(
    commands: &mut mpsc::Receiver<Command>,
    config: &mut TorManagerConfig,
    state: &mut SupervisorState,
    status: &watch::Sender<TorStatus>,
    shutdown: &CancellationToken,
) -> ActiveExitKind {
    loop {
        let command = tokio::select! {
            biased;
            () = shutdown.cancelled() => return ActiveExitKind::Shutdown,
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            return ActiveExitKind::Shutdown;
        };
        match command {
            Command::SetEnabled {
                enabled,
                acknowledged,
            } => {
                state.enabled = enabled;
                let _ = acknowledged.send(());
                if enabled {
                    return ActiveExitKind::Restart;
                }
                publish_status(status, TorStatus::Disabled);
            }
            Command::SetElectrsState {
                target,
                ready,
                acknowledged,
            } => {
                config.target = target;
                state.electrs_ready = ready;
                state.proxy_applied_ready = false;
                state.proxy_revision = state.proxy_revision.wrapping_add(1);
                let _ = acknowledged.send(());
            }
            Command::Retry { acknowledged } => {
                let _ = acknowledged.send(());
            }
            Command::Shutdown { acknowledged } => {
                let _ = acknowledged.send(());
                return ActiveExitKind::Shutdown;
            }
        }
    }
}

async fn run_active_attempt<F>(
    config: &mut TorManagerConfig,
    factory: F,
    retry_policy: RetryPolicy,
    commands: &mut mpsc::Receiver<Command>,
    state: &mut SupervisorState,
    status: &watch::Sender<TorStatus>,
    shutdown: &CancellationToken,
) -> ActiveExit
where
    F: AttemptFactory,
{
    let cancelled = CancellationToken::new();
    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CAPACITY);
    let initial_proxy_state =
        ElectrsProxyState::new(config.target, state.electrs_ready, state.proxy_revision);
    let (proxy_state_tx, proxy_state_rx) = watch::channel(initial_proxy_state);
    let attempt_cancelled = cancelled.clone();
    let mut attempt =
        tokio::spawn(factory.run(config.clone(), event_tx, proxy_state_rx, attempt_cancelled));
    let mut events_open = true;
    let mut pending_proxy_acknowledgements = Vec::new();

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                return finish_active_attempt(
                    &cancelled,
                    &mut attempt,
                    retry_policy.stop_timeout,
                    None,
                    ActiveExitKind::Shutdown,
                ).await;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    return finish_active_attempt(
                        &cancelled,
                        &mut attempt,
                        retry_policy.stop_timeout,
                        None,
                        ActiveExitKind::Shutdown,
                    ).await;
                };
                match command {
                    Command::SetEnabled { enabled, acknowledged } => {
                        if enabled {
                            let _ = acknowledged.send(());
                        } else {
                            return finish_active_attempt(
                                &cancelled,
                                &mut attempt,
                                retry_policy.stop_timeout,
                                Some(acknowledged),
                                ActiveExitKind::Disabled,
                            ).await;
                        }
                    }
                    Command::SetElectrsState { target, ready, acknowledged } => {
                        config.target = target;
                        state.electrs_ready = ready;
                        state.proxy_applied_ready = false;
                        state.proxy_revision = state.proxy_revision.wrapping_add(1);
                        let desired = ElectrsProxyState::new(
                            target,
                            ready,
                            state.proxy_revision,
                        );
                        proxy_state_tx.send_replace(desired);
                        pending_proxy_acknowledgements.push((desired.revision, acknowledged));
                        publish_status(status, state.status());
                    }
                    Command::Retry { acknowledged } => {
                        state.retry_delay = retry_policy.initial;
                        return finish_active_attempt(
                            &cancelled,
                            &mut attempt,
                            retry_policy.stop_timeout,
                            Some(acknowledged),
                            ActiveExitKind::Restart,
                        ).await;
                    }
                    Command::Shutdown { acknowledged } => {
                        return finish_active_attempt(
                            &cancelled,
                            &mut attempt,
                            retry_policy.stop_timeout,
                            Some(acknowledged),
                            ActiveExitKind::Shutdown,
                        ).await;
                    }
                }
            }
            event = event_rx.recv(), if events_open => {
                if let Some(event) = event {
                    apply_attempt_event(
                        event,
                        state,
                        retry_policy,
                        status,
                        &mut pending_proxy_acknowledgements,
                    );
                } else {
                    events_open = false;
                }
            }
            result = &mut attempt => return active_exit_from_attempt(result),
        }
    }
}

fn apply_attempt_event(
    event: AttemptEvent,
    state: &mut SupervisorState,
    retry_policy: RetryPolicy,
    status: &watch::Sender<TorStatus>,
    pending_proxy_acknowledgements: &mut Vec<(u64, oneshot::Sender<()>)>,
) {
    let applied_revision = match &event {
        AttemptEvent::ProxyApplied { revision, .. } => Some(*revision),
        _ => None,
    };
    state.apply(event, retry_policy);
    publish_status(status, state.status());
    if let Some(applied_revision) = applied_revision {
        acknowledge_applied_proxy_state(pending_proxy_acknowledgements, applied_revision);
    }
}

fn active_exit_from_attempt(
    result: Result<Result<(), AttemptFailure>, tokio::task::JoinError>,
) -> ActiveExit {
    match result {
        Ok(Err(failure)) if !failure.relaunch_safe => ActiveExit::UnsafeTeardown(failure),
        Ok(Err(failure)) => ActiveExit::Failed(failure),
        Ok(Ok(())) => ActiveExit::Failed(AttemptFailure::transient(
            "embedded Tor session stopped before it was disabled",
        )),
        Err(error) => ActiveExit::UnsafeTeardown(AttemptFailure::unconfirmed_teardown(format!(
            "The embedded Tor session stopped unexpectedly before onion-service teardown could be confirmed: {error}. Restart BitEngine before enabling Tor again"
        ))),
    }
}

async fn finish_active_attempt(
    cancelled: &CancellationToken,
    attempt: &mut JoinHandle<Result<(), AttemptFailure>>,
    timeout: Duration,
    acknowledged: Option<oneshot::Sender<()>>,
    success: ActiveExitKind,
) -> ActiveExit {
    match stop_attempt(cancelled, attempt, timeout).await {
        Ok(()) => {
            if let Some(acknowledged) = acknowledged {
                let _ = acknowledged.send(());
            }
            ActiveExit::Control(success)
        }
        Err(failure) => ActiveExit::UnsafeTeardown(failure),
    }
}

async fn stop_attempt(
    cancelled: &CancellationToken,
    attempt: &mut JoinHandle<Result<(), AttemptFailure>>,
    timeout: Duration,
) -> Result<(), AttemptFailure> {
    cancelled.cancel();
    match tokio::time::timeout(timeout, &mut *attempt).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(failure))) if failure.relaunch_safe => Ok(()),
        Ok(Ok(Err(failure))) => Err(failure),
        Ok(Err(error)) => Err(AttemptFailure::unconfirmed_teardown(format!(
            "The embedded Tor session stopped unexpectedly before onion-service teardown could be confirmed: {error}. Restart BitEngine before enabling Tor again"
        ))),
        Err(_) => {
            attempt.abort();
            let _ = attempt.await;
            Err(AttemptFailure::unconfirmed_teardown(format!(
                "Onion-service teardown did not finish within {} seconds. Remote state is uncertain; restart BitEngine before enabling Tor again",
                timeout.as_secs()
            )))
        }
    }
}

fn acknowledge_applied_proxy_state(
    pending: &mut Vec<(u64, oneshot::Sender<()>)>,
    applied_revision: u64,
) {
    let applied_count = pending
        .iter()
        .position(|(revision, _)| *revision > applied_revision)
        .unwrap_or(pending.len());
    for (_, acknowledged) in pending.drain(..applied_count) {
        let _ = acknowledged.send(());
    }
}

async fn wait_after_failure(
    retry: Option<Duration>,
    retry_policy: RetryPolicy,
    commands: &mut mpsc::Receiver<Command>,
    config: &mut TorManagerConfig,
    state: &mut SupervisorState,
    status: &watch::Sender<TorStatus>,
    shutdown: &CancellationToken,
) -> ActiveExitKind {
    let retry_sleep = retry.map(tokio::time::sleep);
    tokio::pin!(retry_sleep);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return ActiveExitKind::Shutdown,
            () = async {
                if let Some(sleep) = retry_sleep.as_mut().as_pin_mut() {
                    sleep.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => return ActiveExitKind::Restart,
            command = commands.recv() => {
                let Some(command) = command else {
                    return ActiveExitKind::Shutdown;
                };
                match command {
                    Command::SetEnabled { enabled, acknowledged } => {
                        state.enabled = enabled;
                        let _ = acknowledged.send(());
                        if !enabled {
                            return ActiveExitKind::Disabled;
                        }
                    }
                    Command::SetElectrsState { target, ready, acknowledged } => {
                        config.target = target;
                        state.electrs_ready = ready;
                        state.proxy_applied_ready = false;
                        state.proxy_revision = state.proxy_revision.wrapping_add(1);
                        let _ = acknowledged.send(());
                    }
                    Command::Retry { acknowledged } => {
                        state.retry_delay = retry_policy.initial;
                        let _ = acknowledged.send(());
                        return ActiveExitKind::Restart;
                    }
                    Command::Shutdown { acknowledged } => {
                        let _ = acknowledged.send(());
                        return ActiveExitKind::Shutdown;
                    }
                }
                // Preserve the failure state instead of misleadingly reporting
                // readiness because electrs changed during a Tor outage.
                let current = status.borrow().clone();
                publish_status(status, current);
            }
        }
    }
}

fn publish_status(sender: &watch::Sender<TorStatus>, status: TorStatus) {
    sender.send_replace(status);
}

#[expect(
    clippy::too_many_lines,
    reason = "this linear cancellation-aware lifecycle keeps Arti resources and teardown ordering in one owner"
)]
async fn run_arti_attempt(
    factory: ArtiAttemptFactory,
    config: TorManagerConfig,
    events: mpsc::Sender<AttemptEvent>,
    mut proxy_state: watch::Receiver<ElectrsProxyState>,
    cancelled: CancellationToken,
) -> Result<(), AttemptFailure> {
    let runtime = PreferredRuntime::current().map_err(|error| {
        AttemptFailure::fatal(format!("could not access the Tokio runtime: {error}"))
    })?;
    let client = if let Some(client) = factory.cached_client()? {
        client
    } else {
        // This bounded, local filesystem validation stays inside the owned
        // attempt task. It runs only before the single shared client exists,
        // avoiding both detached blocking work on cancellation and recursive
        // audits racing Arti's live cache updates on later service toggles.
        let storage = prepare_private_storage(&config.storage_root)
            .map_err(|error| AttemptFailure::fatal(format!("Tor storage is unsafe: {error}")))?;
        if cancelled.is_cancelled() {
            return Ok(());
        }
        let arti_config = build_arti_config(&storage)?;
        let client_builder = TorClient::with_runtime(runtime.clone())
            .config(arti_config)
            .bootstrap_behavior(BootstrapBehavior::Manual);
        let created = tokio::select! {
            biased;
            () = cancelled.cancelled() => return Ok(()),
            result = client_builder.create_unbootstrapped_async() => {
                    result.map_err(|error| AttemptFailure::transient(format!(
                        "could not initialize embedded Arti: {error}"
                    )))?
            }
        };
        factory.retain_client(created)?
    };
    client.set_dormant(DormantMode::Normal);
    let _client_dormancy = ArtiClientDormancy(Arc::clone(&client));

    if cancelled.is_cancelled() {
        return Ok(());
    }

    if !emit(
        &events,
        &cancelled,
        AttemptEvent::Bootstrapping {
            progress: bootstrap_percent(client.bootstrap_status().as_frac()),
            summary: bootstrap_summary(&client.bootstrap_status()),
        },
    )
    .await
    {
        return Ok(());
    }

    let mut bootstrap_events = client.bootstrap_events();
    let bootstrap = client.bootstrap();
    tokio::pin!(bootstrap);
    loop {
        tokio::select! {
            biased;
            () = cancelled.cancelled() => return Ok(()),
            result = &mut bootstrap => {
                result.map_err(|error| AttemptFailure::transient(format!(
                    "Arti bootstrap failed: {error}"
                )))?;
                break;
            }
            bootstrap_status = bootstrap_events.next() => {
                if let Some(bootstrap_status) = bootstrap_status {
                    if !emit(
                        &events,
                        &cancelled,
                        AttemptEvent::Bootstrapping {
                            progress: bootstrap_percent(bootstrap_status.as_frac()),
                            summary: bootstrap_summary(&bootstrap_status),
                        },
                    ).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    let nickname = HsNickname::new(SERVICE_NICKNAME.to_owned()).map_err(|error| {
        AttemptFailure::fatal(format!("invalid onion-service identity name: {error}"))
    })?;
    let onion_config = OnionServiceConfigBuilder::default()
        .nickname(nickname.clone())
        .build()
        .map_err(|error| {
            AttemptFailure::fatal(format!("invalid onion-service configuration: {error}"))
        })?;
    if cancelled.is_cancelled() {
        return Ok(());
    }
    // Arti's launch call is synchronous and has no cancellation API. Keep it
    // in this owned attempt task instead of detaching blocking work: disable
    // acknowledgement must wait until either the service handle can be dropped
    // here or the launch has returned an error.
    let launched = client.launch_onion_service(onion_config).map_err(|error| {
        // Arti's public launch error does not reveal whether an earlier
        // publisher/IPT task was already spawned before a later startup step
        // failed. Without a returned service handle/status receiver there is
        // no EOF barrier available, so retrying this nickname in-process would
        // risk overlapping partially launched state.
        AttemptFailure::unconfirmed_teardown(format!(
            "Could not safely finish launching the onion service: {error}. Partial onion-service teardown cannot be confirmed; restart BitEngine before enabling Tor again"
        ))
    })?;
    let Some((service, requests)) = launched else {
        if cancelled.is_cancelled() {
            return Ok(());
        }
        return Err(AttemptFailure::fatal(
            "the fixed BitEngine onion service was unexpectedly disabled",
        ));
    };

    run_owned_electrum_proxy(
        runtime,
        service,
        requests,
        Arc::clone(&client),
        &events,
        &mut proxy_state,
        &cancelled,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixedProxyAction {
    Forward(SocketAddr),
    RejectStream,
    DestroyCircuit,
}

const fn fixed_proxy_action(
    proxy_state: ElectrsProxyState,
    requested_port: Option<u16>,
) -> FixedProxyAction {
    match requested_port {
        Some(ELECTRUM_ONION_PORT) if proxy_state.ready => {
            FixedProxyAction::Forward(proxy_state.target.socket_addr())
        }
        Some(ELECTRUM_ONION_PORT) => FixedProxyAction::RejectStream,
        _ => FixedProxyAction::DestroyCircuit,
    }
}

fn requested_port(request: &StreamRequest) -> Option<u16> {
    match request.request() {
        IncomingStreamRequest::Begin(begin) => Some(begin.port()),
        _ => None,
    }
}

const fn terminal_onion_service_failure(
    state: OnionServiceState,
    has_started: bool,
) -> Option<&'static str> {
    match state {
        OnionServiceState::Broken => Some("Arti reported that onion publication could not recover"),
        OnionServiceState::Shutdown if has_started => {
            Some("Arti stopped the onion service unexpectedly")
        }
        _ => None,
    }
}

async fn run_owned_electrum_proxy<S>(
    runtime: PreferredRuntime,
    service: Arc<RunningOnionService>,
    requests: S,
    client: Arc<TorClient<PreferredRuntime>>,
    events: &mpsc::Sender<AttemptEvent>,
    proxy_state: &mut watch::Receiver<ElectrsProxyState>,
    cancelled: &CancellationToken,
) -> Result<(), AttemptFailure>
where
    S: Stream<Item = RendRequest> + Send,
{
    let mut service_events = service.status_events();
    let outcome = run_electrum_proxy_session(
        runtime,
        &service,
        requests,
        &client,
        events,
        proxy_state,
        cancelled,
        &mut service_events,
    )
    .await;

    drop(service);

    // `RunningOnionService` only signals its detached publisher and
    // introduction-point manager when dropped. Its aggregate `Shutdown` state
    // can appear after either component exits, so wait for status-channel EOF:
    // that proves every status-sender clone owned by those reactors is gone.
    // Disable/retry acknowledgement is downstream of this barrier.
    wait_for_service_status_end(&mut service_events, ONION_SERVICE_STOP_TIMEOUT).await?;
    outcome
}

#[expect(
    clippy::too_many_arguments,
    reason = "explicit borrowed lifecycle resources keep the single fixed proxy session independently testable"
)]
async fn run_electrum_proxy_session<S>(
    runtime: PreferredRuntime,
    service: &RunningOnionService,
    requests: S,
    client: &TorClient<PreferredRuntime>,
    events: &mpsc::Sender<AttemptEvent>,
    proxy_state: &mut watch::Receiver<ElectrsProxyState>,
    cancelled: &CancellationToken,
    service_events: &mut OnionServiceStatusStream,
) -> Result<(), AttemptFailure>
where
    S: Stream<Item = RendRequest> + Send,
{
    let hsid = service.onion_address().ok_or_else(|| {
        AttemptFailure::transient("Arti did not return the persistent onion identity")
    })?;
    if !emit(
        events,
        cancelled,
        AttemptEvent::Identity(OnionHostname::from_hsid(hsid)),
    )
    .await
    {
        return Ok(());
    }

    let mut stream_requests = Box::pin(tor_hsservice::handle_rend_requests(requests));
    let mut connections = FuturesUnordered::new();
    let mut client_events = client.bootstrap_events();
    let mut service_state = service.status().state();
    let mut client_ready = client.bootstrap_status().ready_for_traffic();
    // A newly launched Arti service begins with a legitimate `Shutdown`
    // snapshot until its reactor first reports Bootstrapping. Only treat a
    // later return to Shutdown as terminal.
    let mut service_started = service_state != OnionServiceState::Shutdown;
    if let Some(message) = terminal_onion_service_failure(service_state, service_started) {
        return Err(AttemptFailure::transient(message));
    }

    if !emit_initial_proxy_state(events, cancelled, proxy_state).await {
        return Ok(());
    }

    if service_started && !emit_service_state(events, cancelled, service_state, client_ready).await
    {
        return Ok(());
    }

    let outcome = loop {
        tokio::select! {
            biased;
            () = cancelled.cancelled() => break Ok(()),
            changed = proxy_state.changed() => {
                if changed.is_err() {
                    break Err(AttemptFailure::transient(
                        "managed electrs proxy-state updates stopped unexpectedly",
                    ));
                }
                let applied = *proxy_state.borrow_and_update();
                revoke_proxy_connections(&mut connections);
                if !emit(
                    events,
                    cancelled,
                    AttemptEvent::ProxyApplied {
                        revision: applied.revision,
                        ready: applied.ready,
                    },
                ).await {
                    break Ok(());
                }
            }
            next = service_events.next() => {
                let Some(next) = next else {
                    break Err(AttemptFailure::transient(
                        "Arti stopped reporting onion-service state",
                    ));
                };
                service_state = next.state();
                if let Some(message) = terminal_onion_service_failure(service_state, service_started) {
                    break Err(AttemptFailure::transient(message));
                }
                if service_state == OnionServiceState::Shutdown {
                    continue;
                }
                service_started = true;
                if !emit_service_state(events, cancelled, service_state, client_ready).await {
                    break Ok(());
                }
            }
            next = client_events.next() => {
                let Some(next) = next else {
                    break Err(AttemptFailure::transient(
                        "Arti stopped reporting Tor network state",
                    ));
                };
                client_ready = next.ready_for_traffic();
                if service_started
                    && !emit_service_state(events, cancelled, service_state, client_ready).await
                {
                    break Ok(());
                }
            }
            _ = connections.next(), if !connections.is_empty() => {}
            request = stream_requests.next() => {
                let Some(request) = request else {
                    break Err(AttemptFailure::transient(
                        "onion-service request stream ended unexpectedly",
                    ));
                };
                let current_proxy_state = *proxy_state.borrow();
                let action = fixed_proxy_action(current_proxy_state, requested_port(&request));
                connections.push(handle_electrum_stream(runtime.clone(), request, action));
            }
        }
    };

    // Clear every accepted/in-progress stream before this helper drops the
    // request adapter and its pending rendezvous work.
    revoke_proxy_connections(&mut connections);
    outcome
}

async fn emit_initial_proxy_state(
    events: &mpsc::Sender<AttemptEvent>,
    cancelled: &CancellationToken,
    proxy_state: &mut watch::Receiver<ElectrsProxyState>,
) -> bool {
    let initial = *proxy_state.borrow_and_update();
    emit(
        events,
        cancelled,
        AttemptEvent::ProxyApplied {
            revision: initial.revision,
            ready: initial.ready,
        },
    )
    .await
}

async fn handle_electrum_stream(
    runtime: PreferredRuntime,
    request: StreamRequest,
    action: FixedProxyAction,
) {
    match action {
        FixedProxyAction::RejectStream => {
            let _ = request.reject(End::new_with_reason(EndReason::DONE)).await;
        }
        FixedProxyAction::DestroyCircuit => {
            let _ = request.shutdown_circuit();
        }
        FixedProxyAction::Forward(target) => {
            forward_electrum_stream(runtime, request, target).await;
        }
    }
}

async fn forward_electrum_stream(
    runtime: PreferredRuntime,
    request: StreamRequest,
    target: SocketAddr,
) {
    let connect_options =
        <PreferredRuntime as tor_rtcompat::NetStreamProvider<SocketAddr>>::ConnectOptions::default(
        );
    let Ok(local_stream) = runtime.connect(&target, &connect_options).await else {
        let _ = request.reject(End::new_with_reason(EndReason::DONE)).await;
        return;
    };
    let Ok(onion_stream) = request.accept(Connected::new_empty()).await else {
        return;
    };
    let _ = futures_copy::copy_bidirectional(
        onion_stream,
        local_stream,
        futures_copy::eof::Close,
        futures_copy::eof::Close,
    )
    .await;
}

fn revoke_proxy_connections<F>(connections: &mut FuturesUnordered<F>) {
    connections.clear();
}

async fn wait_for_service_status_end<S>(
    status_events: &mut S,
    timeout: Duration,
) -> Result<(), AttemptFailure>
where
    S: Stream + Unpin,
{
    tokio::time::timeout(timeout, async {
        while status_events.next().await.is_some() {}
    })
    .await
    .map_err(|_| {
        AttemptFailure::unconfirmed_teardown(format!(
            "Arti's onion-service reactors did not stop within {} seconds. Remote state is uncertain; restart BitEngine before enabling Tor again",
            timeout.as_secs()
        ))
    })
}

struct ArtiClientDormancy(Arc<TorClient<PreferredRuntime>>);

impl Drop for ArtiClientDormancy {
    fn drop(&mut self) {
        self.0.set_dormant(DormantMode::Soft);
    }
}

async fn emit_service_state(
    events: &mpsc::Sender<AttemptEvent>,
    cancelled: &CancellationToken,
    service_state: OnionServiceState,
    client_ready: bool,
) -> bool {
    let event = if client_ready {
        match service_state {
            OnionServiceState::Bootstrapping => AttemptEvent::Publishing,
            OnionServiceState::Running | OnionServiceState::DegradedReachable => {
                AttemptEvent::Reachable
            }
            OnionServiceState::Recovering => AttemptEvent::TemporarilyUnavailable(
                "The onion service is recovering its introduction points".to_owned(),
            ),
            OnionServiceState::DegradedUnreachable => AttemptEvent::TemporarilyUnavailable(
                "The onion descriptor could not reach enough Tor directories yet".to_owned(),
            ),
            OnionServiceState::Broken => AttemptEvent::TemporarilyUnavailable(
                "Arti reported that onion publication is unavailable".to_owned(),
            ),
            OnionServiceState::Shutdown => AttemptEvent::TemporarilyUnavailable(
                "The onion service stopped unexpectedly".to_owned(),
            ),
            _ => AttemptEvent::TemporarilyUnavailable(
                "The onion service is temporarily unavailable".to_owned(),
            ),
        }
    } else {
        AttemptEvent::TemporarilyUnavailable(
            "Tor network connectivity is interrupted; Arti is recovering".to_owned(),
        )
    };
    emit(events, cancelled, event).await
}

async fn emit(
    events: &mpsc::Sender<AttemptEvent>,
    cancelled: &CancellationToken,
    event: AttemptEvent,
) -> bool {
    tokio::select! {
        biased;
        () = cancelled.cancelled() => false,
        result = events.send(event) => result.is_ok(),
    }
}

fn build_arti_config(storage: &StorageLayout) -> Result<TorClientConfig, AttemptFailure> {
    let mut builder = TorClientConfigBuilder::from_directories(&storage.state, &storage.cache);
    // Do not let untrusted process environment variables disable Arti's
    // permission checks.  Group access is also disallowed for identity/state.
    builder
        .storage()
        .permissions()
        .ignore_environment()
        .trust_no_group_id();
    builder.build().map_err(|error| {
        AttemptFailure::fatal(format!("invalid Arti storage configuration: {error}"))
    })
}

#[derive(Debug, Clone)]
struct StorageLayout {
    state: PathBuf,
    cache: PathBuf,
}

fn prepare_private_storage(root: &Path) -> Result<StorageLayout, StorageError> {
    validate_storage_root_lexically(root).map_err(StorageError::Configuration)?;
    reject_symlink_components(root)?;
    let mistrust = strict_mistrust()?;
    mistrust
        .make_directory(root)
        .map_err(StorageError::Mistrust)?;
    let state = root.join("state");
    let cache = root.join("cache");
    mistrust
        .make_directory(&state)
        .map_err(StorageError::Mistrust)?;
    mistrust
        .make_directory(&cache)
        .map_err(StorageError::Mistrust)?;
    prepare_arti_lock_files(&state, &cache, &mistrust)?;
    // check_content rejects symlinks and unsafe writable content recursively.
    // The explicit audit additionally requires exact current-user ownership,
    // private directories and state, bounded traversal, and no hard links.
    // Arti's public-consensus cache may contain read-only 0644 files by design.
    mistrust
        .verifier()
        .check_content()
        .check(root)
        .map_err(StorageError::Mistrust)?;
    audit_private_tree(root, &cache)?;
    Ok(StorageLayout { state, cache })
}

#[cfg(unix)]
fn prepare_arti_lock_files(
    state: &Path,
    cache: &Path,
    mistrust: &Mistrust,
) -> Result<(), StorageError> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let service_directory = state.join("hss");
    mistrust
        .make_directory(&service_directory)
        .map_err(StorageError::Mistrust)?;
    let persistent_state_directory = state.join("state");
    mistrust
        .make_directory(&persistent_state_directory)
        .map_err(StorageError::Mistrust)?;

    for lock_path in arti_lock_paths(state, cache) {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let lock = options
            .open(&lock_path)
            .map_err(|source| StorageError::Inspect {
                path: lock_path.clone(),
                source,
            })?;
        let metadata = lock.metadata().map_err(|source| StorageError::Inspect {
            path: lock_path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(StorageError::UnexpectedObject(lock_path));
        }
        // SAFETY: `geteuid` has no preconditions and only returns process state.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(StorageError::WrongOwner(lock_path));
        }
        if metadata.nlink() != 1 {
            return Err(StorageError::HardLinkedFile(lock_path));
        }
        if metadata.mode() & 0o7777 != 0o600 {
            lock.set_permissions(Permissions::from_mode(0o600))
                .map_err(|source| StorageError::Inspect {
                    path: lock_path,
                    source,
                })?;
        }
    }
    Ok(())
}

fn arti_lock_paths(state: &Path, cache: &Path) -> [PathBuf; 3] {
    [
        cache.join("dir.lock"),
        state.join("state/state.lock"),
        state.join("hss").join(format!("{SERVICE_NICKNAME}.lock")),
    ]
}

#[cfg(not(unix))]
fn prepare_arti_lock_files(_: &Path, _: &Path, _: &Mistrust) -> Result<(), StorageError> {
    Err(StorageError::UnsupportedPlatform)
}

fn strict_mistrust() -> Result<Mistrust, StorageError> {
    let mut builder = Mistrust::builder();
    builder.ignore_environment().trust_no_group_id();
    builder
        .build()
        .map_err(|error| StorageError::MistrustConfiguration(error.to_string()))
}

fn validate_storage_root_lexically(path: &Path) -> Result<(), TorConfigError> {
    if !path.is_absolute() {
        return Err(TorConfigError::InvalidStorageRoot {
            problem: "path must be absolute".to_owned(),
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(TorConfigError::InvalidStorageRoot {
            problem: "path must not contain . or ..".to_owned(),
        });
    }
    if path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
        < 2
    {
        return Err(TorConfigError::InvalidStorageRoot {
            problem: "path is too close to the filesystem root".to_owned(),
        });
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), StorageError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StorageError::SymbolicLink(current));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(StorageError::UnexpectedObject(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(StorageError::Inspect {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn audit_private_tree(root: &Path, cache: &Path) -> Result<(), StorageError> {
    use std::os::unix::fs::MetadataExt as _;

    // SAFETY: `geteuid` has no preconditions and only returns process state.
    let expected_user = unsafe { libc::geteuid() };
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut entries = 0_usize;
    while let Some((path, depth)) = pending.pop() {
        entries = entries.saturating_add(1);
        if entries > MAX_STORAGE_ENTRIES || depth > MAX_STORAGE_DEPTH {
            return Err(StorageError::TraversalLimit);
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| StorageError::Inspect {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(StorageError::SymbolicLink(path));
        }
        if metadata.uid() != expected_user {
            return Err(StorageError::WrongOwner(path));
        }
        let mode = metadata.mode();
        let public_cache_file = metadata.is_file() && path.starts_with(cache);
        let unsafe_permissions = mode & 0o7000 != 0
            || if public_cache_file {
                // Tor directory documents are public information. Arti's
                // SQLite cache intentionally uses 0644, but it must never be
                // writable or executable by another user.
                mode & 0o033 != 0
            } else {
                mode & 0o077 != 0
            };
        if unsafe_permissions {
            return Err(StorageError::UnsafePermissions(path));
        }
        if metadata.is_dir() {
            let directory = std::fs::read_dir(&path).map_err(|source| StorageError::Inspect {
                path: path.clone(),
                source,
            })?;
            for entry in directory {
                let entry = entry.map_err(|source| StorageError::Inspect {
                    path: path.clone(),
                    source,
                })?;
                pending.push((entry.path(), depth.saturating_add(1)));
            }
        } else if metadata.is_file() {
            if metadata.nlink() != 1 {
                return Err(StorageError::HardLinkedFile(path));
            }
        } else {
            return Err(StorageError::UnexpectedObject(path));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn audit_private_tree(_: &Path, _: &Path) -> Result<(), StorageError> {
    Err(StorageError::UnsupportedPlatform)
}

#[derive(Debug, Error)]
enum StorageError {
    #[error(transparent)]
    Configuration(#[from] TorConfigError),
    #[error("filesystem permission validation failed: {0}")]
    Mistrust(#[source] fs_mistrust::Error),
    #[error("could not construct strict filesystem validation: {0}")]
    MistrustConfiguration(String),
    #[error("Tor storage contains a symbolic link: {0}")]
    SymbolicLink(PathBuf),
    #[error("Tor storage contains an unexpected filesystem object: {0}")]
    UnexpectedObject(PathBuf),
    #[error("Tor storage is not owned by the current user: {0}")]
    WrongOwner(PathBuf),
    #[error("Tor storage permits group or world access: {0}")]
    UnsafePermissions(PathBuf),
    #[error("Tor storage contains a hard-linked file: {0}")]
    HardLinkedFile(PathBuf),
    #[error("Tor storage exceeds the safe audit traversal limit")]
    TraversalLimit,
    #[error("could not inspect Tor storage at {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(not(unix))]
    #[error("private Arti storage is currently supported only on macOS and Linux")]
    UnsupportedPlatform,
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is explicitly clamped to the inclusive u8 percentage range"
)]
fn bootstrap_percent(fraction: f32) -> u8 {
    if !fraction.is_finite() || fraction <= 0.0 {
        0
    } else if fraction >= 1.0 {
        100
    } else {
        (fraction * 100.0).round() as u8
    }
}

fn bootstrap_summary(status: &arti_client::status::BootstrapStatus) -> Option<String> {
    status
        .blocked()
        .map(|blockage| bounded_diagnostic(blockage.message().to_string()))
}

fn bounded_diagnostic(message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return message.to_owned();
    }
    let mut bounded = message
        .chars()
        .take(MAX_DIAGNOSTIC_CHARS)
        .collect::<String>();
    bounded.push('…');
    bounded
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        net::{Ipv4Addr, SocketAddrV4, SocketAddrV6},
        os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
        os::unix::net::UnixListener,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };

    use anyhow::{Context as _, Result};
    use tokio::sync::broadcast;
    use tor_keymgr::KeystoreSelector;

    use super::*;

    const TEST_ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

    #[derive(Debug, Clone)]
    enum FakeAction {
        Reachable,
        Unavailable(&'static str),
        Fail(&'static str),
    }

    #[derive(Debug, Clone)]
    struct FakeAttemptFactory {
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        failures_remaining: Arc<AtomicUsize>,
        fail_next_proxy_update: Arc<AtomicBool>,
        control: broadcast::Sender<FakeAction>,
        proxy_states: Arc<StdMutex<Vec<ElectrsProxyState>>>,
    }

    impl FakeAttemptFactory {
        fn new(failures: usize) -> Self {
            let (control, _) = broadcast::channel(16);
            Self {
                starts: Arc::new(AtomicUsize::new(0)),
                stops: Arc::new(AtomicUsize::new(0)),
                failures_remaining: Arc::new(AtomicUsize::new(failures)),
                fail_next_proxy_update: Arc::new(AtomicBool::new(false)),
                control,
                proxy_states: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn send(&self, action: FakeAction) -> Result<()> {
            self.control
                .send(action)
                .map(|_| ())
                .context("fake attempt is not listening")
        }

        fn starts(&self) -> usize {
            self.starts.load(Ordering::Acquire)
        }

        fn stops(&self) -> usize {
            self.stops.load(Ordering::Acquire)
        }

        fn fail_next_proxy_update(&self) {
            self.fail_next_proxy_update.store(true, Ordering::Release);
        }

        fn proxy_states(&self) -> Result<Vec<ElectrsProxyState>> {
            self.proxy_states
                .lock()
                .map(|states| states.clone())
                .map_err(|_| anyhow::anyhow!("fake proxy-state log lock was poisoned"))
        }
    }

    struct FakeStopGuard(Arc<AtomicUsize>);

    impl Drop for FakeStopGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct ProxyTaskDropGuard(Arc<AtomicBool>);

    impl Drop for ProxyTaskDropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    async fn pending_proxy_task(dropped: Arc<AtomicBool>, started: oneshot::Sender<()>) {
        let _guard = ProxyTaskDropGuard(dropped);
        let _ = started.send(());
        std::future::pending::<()>().await;
    }

    async fn pending_attempt_task(
        dropped: Arc<AtomicBool>,
        started: oneshot::Sender<()>,
    ) -> Result<(), AttemptFailure> {
        let _guard = ProxyTaskDropGuard(dropped);
        let _ = started.send(());
        std::future::pending::<Result<(), AttemptFailure>>().await
    }

    #[derive(Clone)]
    struct TeardownLatchAttemptFactory {
        starts: Arc<AtomicUsize>,
        cancellations: Arc<AtomicUsize>,
        completions: Arc<AtomicUsize>,
        release: watch::Receiver<bool>,
    }

    impl TeardownLatchAttemptFactory {
        fn new(release: watch::Receiver<bool>) -> Self {
            Self {
                starts: Arc::new(AtomicUsize::new(0)),
                cancellations: Arc::new(AtomicUsize::new(0)),
                completions: Arc::new(AtomicUsize::new(0)),
                release,
            }
        }
    }

    impl AttemptFactory for TeardownLatchAttemptFactory {
        async fn run(
            self,
            _config: TorManagerConfig,
            _events: mpsc::Sender<AttemptEvent>,
            _proxy_state: watch::Receiver<ElectrsProxyState>,
            cancelled: CancellationToken,
        ) -> Result<(), AttemptFailure> {
            let mut release = self.release.clone();
            self.starts.fetch_add(1, Ordering::AcqRel);
            cancelled.cancelled().await;
            self.cancellations.fetch_add(1, Ordering::AcqRel);
            loop {
                let released = *release.borrow_and_update();
                if released {
                    break;
                }
                release.changed().await.map_err(|_| {
                    AttemptFailure::unconfirmed_teardown(
                        "fake onion-service teardown release channel stopped",
                    )
                })?;
            }
            self.completions.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    impl AttemptFactory for FakeAttemptFactory {
        async fn run(
            self,
            _config: TorManagerConfig,
            events: mpsc::Sender<AttemptEvent>,
            mut proxy_state: watch::Receiver<ElectrsProxyState>,
            cancelled: CancellationToken,
        ) -> Result<(), AttemptFailure> {
            let mut control = self.control.subscribe();
            self.starts.fetch_add(1, Ordering::AcqRel);
            let _stop_guard = FakeStopGuard(Arc::clone(&self.stops));
            if self
                .failures_remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(AttemptFailure::transient(
                    "scripted onion descriptor startup failure",
                ));
            }

            let initial_proxy_state = *proxy_state.borrow();
            record_proxy_state(&self.proxy_states, initial_proxy_state)?;
            for event in [
                AttemptEvent::Bootstrapping {
                    progress: 50,
                    summary: None,
                },
                AttemptEvent::Identity(OnionHostname::parse(TEST_ONION_HOST)?),
                AttemptEvent::Publishing,
                AttemptEvent::ProxyApplied {
                    revision: initial_proxy_state.revision,
                    ready: initial_proxy_state.ready,
                },
                AttemptEvent::Reachable,
            ] {
                if !emit(&events, &cancelled, event).await {
                    return Ok(());
                }
            }

            loop {
                tokio::select! {
                    biased;
                    () = cancelled.cancelled() => return Ok(()),
                    changed = proxy_state.changed() => {
                        if changed.is_err() {
                            return Err(AttemptFailure::transient(
                                "fake proxy-state updates stopped",
                            ));
                        }
                        let applied = *proxy_state.borrow_and_update();
                        record_proxy_state(&self.proxy_states, applied)?;
                        if self.fail_next_proxy_update.swap(false, Ordering::AcqRel) {
                            return Err(AttemptFailure::transient(
                                "scripted failure before proxy-state application",
                            ));
                        }
                        if !emit(
                            &events,
                            &cancelled,
                            AttemptEvent::ProxyApplied {
                                revision: applied.revision,
                                ready: applied.ready,
                            },
                        ).await {
                            return Ok(());
                        }
                    }
                    action = control.recv() => {
                        match action {
                            Ok(FakeAction::Reachable) => {
                                if !emit(&events, &cancelled, AttemptEvent::Reachable).await {
                                    return Ok(());
                                }
                            }
                            Ok(FakeAction::Unavailable(message)) => {
                                if !emit(
                                    &events,
                                    &cancelled,
                                    AttemptEvent::TemporarilyUnavailable(message.to_owned()),
                                ).await {
                                    return Ok(());
                                }
                            }
                            Ok(FakeAction::Fail(message)) => {
                                return Err(AttemptFailure::transient(message));
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => {
                                return Err(AttemptFailure::transient(
                                    "fake control stream stopped",
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn record_proxy_state(
        states: &StdMutex<Vec<ElectrsProxyState>>,
        state: ElectrsProxyState,
    ) -> Result<(), AttemptFailure> {
        states
            .lock()
            .map_err(|_| AttemptFailure::fatal("fake proxy-state log lock was poisoned"))?
            .push(state);
        Ok(())
    }

    fn test_retry_policy() -> RetryPolicy {
        RetryPolicy {
            initial: Duration::from_millis(25),
            maximum: Duration::from_millis(100),
            stop_timeout: Duration::from_secs(1),
        }
    }

    fn loopback_target() -> Result<ElectrsTorTarget> {
        ElectrsTorTarget::new(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            ELECTRUM_ONION_PORT,
        )))
        .map_err(Into::into)
    }

    fn test_manager_config(
        directory: &tempfile::TempDir,
        enabled: bool,
        electrs_ready: bool,
    ) -> Result<TorManagerConfig> {
        let canonical = directory.path().canonicalize()?;
        TorManagerConfig::new(canonical.join("bitengine/tor"), loopback_target()?)
            .map(|config| {
                config
                    .initially_enabled(enabled)
                    .initially_electrs_ready(electrs_ready)
            })
            .map_err(Into::into)
    }

    async fn wait_for_status(
        manager: &TorManager,
        predicate: impl Fn(&TorStatus) -> bool + Send + Sync,
    ) -> Result<TorStatus> {
        let mut status = manager.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let current = status.borrow().clone();
                if predicate(&current) {
                    return Ok(current);
                }
                status
                    .changed()
                    .await
                    .context("Tor status channel stopped")?;
            }
        })
        .await
        .context("timed out waiting for Tor status")?
    }

    async fn wait_for_count(counter: &AtomicUsize, minimum: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            while counter.load(Ordering::Acquire) < minimum {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("timed out waiting for fake attempt count")
    }

    #[test]
    fn target_type_prevents_rpc_metrics_public_and_wildcard_exposure() -> Result<()> {
        for allowed in [
            "127.0.0.1:50001",
            "[::1]:50001",
            "10.0.0.8:50001",
            "172.16.1.9:50001",
            "192.168.42.2:50001",
            "[fd12:3456::8]:50001",
        ] {
            let address = allowed.parse::<SocketAddr>()?;
            assert_eq!(ElectrsTorTarget::new(address)?.socket_addr(), address);
        }

        for rejected in [
            "0.0.0.0:50001",
            "[::]:50001",
            "8.8.8.8:50001",
            "169.254.1.2:50001",
            "[fe80::1]:50001",
            "127.0.0.1:8332",
            "127.0.0.1:4224",
            "192.168.1.5:8332",
        ] {
            let address = rejected.parse::<SocketAddr>()?;
            assert!(
                ElectrsTorTarget::new(address).is_err(),
                "accepted {address}"
            );
        }

        let target = loopback_target()?;
        let not_ready = ElectrsProxyState::new(target, false, 0);
        assert_eq!(
            fixed_proxy_action(not_ready, Some(ELECTRUM_ONION_PORT)),
            FixedProxyAction::RejectStream
        );

        let ready = ElectrsProxyState::new(target, true, 1);
        assert_eq!(
            fixed_proxy_action(ready, Some(ELECTRUM_ONION_PORT)),
            FixedProxyAction::Forward(target.socket_addr())
        );
        for port in [Some(8332), Some(4224), None] {
            assert_eq!(
                fixed_proxy_action(ready, port),
                FixedProxyAction::DestroyCircuit
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn proxy_revocation_synchronously_drops_active_sessions() -> Result<()> {
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = oneshot::channel();
        let mut connections = FuturesUnordered::new();
        connections.push(pending_proxy_task(Arc::clone(&dropped), started_tx));

        tokio::select! {
            started = &mut started_rx => started?,
            _ = connections.next() => anyhow::bail!("pending proxy task exited unexpectedly"),
        }
        revoke_proxy_connections(&mut connections);

        assert!(connections.is_empty());
        assert!(dropped.load(Ordering::Acquire));
        Ok(())
    }

    #[tokio::test]
    async fn active_parent_drop_synchronously_revokes_polled_sessions() -> Result<()> {
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = oneshot::channel();
        let mut connections = FuturesUnordered::new();
        connections.push(pending_proxy_task(Arc::clone(&dropped), started_tx));

        tokio::select! {
            started = &mut started_rx => started?,
            _ = connections.next() => anyhow::bail!("pending proxy task exited unexpectedly"),
        }
        drop(connections);

        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 1);
        Ok(())
    }

    #[tokio::test]
    async fn forced_attempt_abort_waits_for_owned_resource_teardown() -> Result<()> {
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let mut attempt = tokio::spawn(pending_attempt_task(Arc::clone(&dropped), started_tx));
        started_rx.await?;

        let stopped = stop_attempt(&CancellationToken::new(), &mut attempt, Duration::ZERO).await;

        assert!(stopped.is_err());
        assert!(attempt.is_finished());
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&dropped), 1);
        Ok(())
    }

    #[tokio::test]
    async fn service_status_eof_waits_for_every_reactor_sender() -> Result<()> {
        let (sender, mut status_events) = futures::channel::mpsc::unbounded::<()>();
        let barrier = tokio::spawn(async move {
            wait_for_service_status_end(&mut status_events, Duration::from_secs(1)).await
        });

        tokio::task::yield_now().await;
        assert!(!barrier.is_finished());
        sender.unbounded_send(())?;
        tokio::task::yield_now().await;
        assert!(
            !barrier.is_finished(),
            "a status value was mistaken for complete reactor teardown"
        );

        drop(sender);
        let result = barrier.await?;
        assert!(result.is_ok(), "status EOF barrier failed: {result:?}");
        Ok(())
    }

    #[tokio::test]
    async fn service_status_eof_timeout_marks_relaunch_unsafe() -> Result<()> {
        let (_sender, mut status_events) = futures::channel::mpsc::unbounded::<()>();
        let result = wait_for_service_status_end(&mut status_events, Duration::ZERO).await;
        let Err(failure) = result else {
            anyhow::bail!("open status channel unexpectedly passed the teardown barrier");
        };
        assert!(!failure.retryable);
        assert!(!failure.relaunch_safe);
        assert!(failure.message.contains("restart BitEngine"));
        Ok(())
    }

    #[tokio::test]
    async fn queued_reenable_waits_for_complete_attempt_teardown() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, false)?;
        let (release_tx, release_rx) = watch::channel(false);
        let factory = TeardownLatchAttemptFactory::new(release_rx);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        wait_for_count(&factory.starts, 1).await?;

        let (disable_acknowledged, mut disable_response) = oneshot::channel();
        manager.inner.commands.try_send(Command::SetEnabled {
            enabled: false,
            acknowledged: disable_acknowledged,
        })?;
        wait_for_count(&factory.cancellations, 1).await?;
        let (enable_acknowledged, mut enable_response) = oneshot::channel();
        manager.inner.commands.try_send(Command::SetEnabled {
            enabled: true,
            acknowledged: enable_acknowledged,
        })?;

        assert!(matches!(
            disable_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            enable_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(factory.starts.load(Ordering::Acquire), 1);
        assert_eq!(factory.completions.load(Ordering::Acquire), 0);

        release_tx.send_replace(true);
        disable_response.await?;
        assert_eq!(factory.completions.load(Ordering::Acquire), 1);
        enable_response.await?;
        wait_for_count(&factory.starts, 2).await?;
        manager.shutdown().await?;
        assert_eq!(factory.completions.load(Ordering::Acquire), 2);
        Ok(())
    }

    #[tokio::test]
    async fn teardown_timeout_closes_worker_and_blocks_same_nickname_relaunch() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, false)?;
        let (_release_tx, release_rx) = watch::channel(false);
        let factory = TeardownLatchAttemptFactory::new(release_rx);
        let retry_policy = RetryPolicy {
            stop_timeout: Duration::from_secs(1),
            ..test_retry_policy()
        };
        let manager = TorManager::spawn_with_factory(config, factory.clone(), retry_policy)?;
        wait_for_count(&factory.starts, 1).await?;

        let (disable_acknowledged, disable_response) = oneshot::channel();
        manager.inner.commands.try_send(Command::SetEnabled {
            enabled: false,
            acknowledged: disable_acknowledged,
        })?;
        let (enable_acknowledged, enable_response) = oneshot::channel();
        manager.inner.commands.try_send(Command::SetEnabled {
            enabled: true,
            acknowledged: enable_acknowledged,
        })?;
        wait_for_count(&factory.cancellations, 1).await?;

        assert!(disable_response.await.is_err());
        assert!(enable_response.await.is_err());
        let error =
            wait_for_status(&manager, |status| matches!(status, TorStatus::Error { .. })).await?;
        let TorStatus::Error { message, retryable } = error else {
            anyhow::bail!("teardown timeout did not remain visible as a fatal error");
        };
        assert!(!retryable);
        assert!(message.contains("restart BitEngine"));
        assert_eq!(
            manager.set_enabled(true).await,
            Err(TorControlError::WorkerStopped)
        );
        assert_eq!(factory.starts.load(Ordering::Acquire), 1);
        assert_eq!(factory.completions.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[test]
    fn proxy_action_never_forwards_to_an_unvalidated_target() -> Result<()> {
        let target = ElectrsTorTarget::new("192.168.50.7:50001".parse()?)?;
        let ready = ElectrsProxyState::new(target, true, u64::MAX);
        assert_eq!(
            fixed_proxy_action(ready, Some(ELECTRUM_ONION_PORT)),
            FixedProxyAction::Forward(target.socket_addr())
        );
        Ok(())
    }

    #[test]
    fn terminal_onion_service_states_restart_the_owned_proxy() {
        assert!(terminal_onion_service_failure(OnionServiceState::Broken, false).is_some());
        assert!(terminal_onion_service_failure(OnionServiceState::Broken, true).is_some());
        assert!(terminal_onion_service_failure(OnionServiceState::Shutdown, false).is_none());
        assert!(terminal_onion_service_failure(OnionServiceState::Shutdown, true).is_some());
        for recoverable in [
            OnionServiceState::Bootstrapping,
            OnionServiceState::DegradedReachable,
            OnionServiceState::DegradedUnreachable,
            OnionServiceState::Running,
            OnionServiceState::Recovering,
        ] {
            assert!(terminal_onion_service_failure(recoverable, false).is_none());
            assert!(terminal_onion_service_failure(recoverable, true).is_none());
        }
    }

    #[tokio::test]
    async fn latest_false_snapshot_coalesces_and_acknowledges_superseded_true() -> Result<()> {
        let target = loopback_target()?;
        let (proxy_state_tx, mut proxy_state_rx) =
            watch::channel(ElectrsProxyState::new(target, false, 0));
        let (true_ack, mut true_response) = oneshot::channel();
        let (false_ack, mut false_response) = oneshot::channel();
        let mut pending = vec![(1, true_ack), (2, false_ack)];

        proxy_state_tx.send_replace(ElectrsProxyState::new(target, true, 1));
        proxy_state_tx.send_replace(ElectrsProxyState::new(target, false, 2));
        proxy_state_rx.changed().await?;
        let applied = *proxy_state_rx.borrow_and_update();
        assert_eq!(applied, ElectrsProxyState::new(target, false, 2));

        assert_eq!(
            fixed_proxy_action(applied, Some(ELECTRUM_ONION_PORT)),
            FixedProxyAction::RejectStream
        );
        acknowledge_applied_proxy_state(&mut pending, applied.revision);
        assert!(pending.is_empty());
        assert_eq!(true_response.try_recv(), Ok(()));
        assert_eq!(false_response.try_recv(), Ok(()));
        Ok(())
    }

    #[test]
    fn production_storage_is_derived_beside_config_and_not_user_configurable() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let base = temporary.path().canonicalize()?.join("BitEngine");
        let config =
            TorManagerConfig::for_bitengine_config(&base.join("config.json"), loopback_target()?)?;
        assert_eq!(config.storage_root(), base.join("tor"));
        Ok(())
    }

    #[test]
    fn storage_rejects_relative_traversal_symlinks_and_permissive_modes() -> Result<()> {
        assert!(TorManagerConfig::new(PathBuf::from("relative/tor"), loopback_target()?).is_err());
        assert!(
            TorManagerConfig::new(PathBuf::from("/tmp/../unsafe"), loopback_target()?).is_err()
        );

        let temporary = tempfile::tempdir()?;
        let base = temporary.path().canonicalize()?;
        let root = base.join("private/tor");
        let layout = prepare_private_storage(&root)?;
        assert!(layout.state.is_dir());
        assert!(layout.cache.is_dir());

        let mut permissions = std::fs::metadata(&root)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&root, permissions)?;
        assert!(prepare_private_storage(&root).is_err());

        let alias = base.join("alias");
        std::os::unix::fs::symlink(&root, &alias)?;
        assert!(prepare_private_storage(&alias).is_err());
        Ok(())
    }

    #[test]
    fn storage_allows_public_read_only_cache_but_keeps_state_private() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?.join("private/tor");
        let storage = prepare_private_storage(&root)?;

        let cache_file = storage.cache.join("public-directory-cache");
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .mode(0o600)
            .truncate(false);
        drop(options.open(&cache_file)?);
        let mut permissions = std::fs::metadata(&cache_file)?.permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&cache_file, permissions)?;
        prepare_private_storage(&root)?;

        let state_file = storage.state.join("private-state");
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .mode(0o600)
            .truncate(false);
        drop(options.open(&state_file)?);
        let mut permissions = std::fs::metadata(&state_file)?.permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&state_file, permissions)?;
        assert!(prepare_private_storage(&root).is_err());

        let mut permissions = std::fs::metadata(&state_file)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&state_file, permissions)?;
        let mut permissions = std::fs::metadata(&cache_file)?.permissions();
        permissions.set_mode(0o664);
        std::fs::set_permissions(&cache_file, permissions)?;
        assert!(prepare_private_storage(&root).is_err());
        Ok(())
    }

    #[test]
    fn storage_rejects_special_files_hard_links_and_permission_bypass() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?.join("private/tor");
        prepare_private_storage(&root)?;
        let first = root.join("state/identity-material");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).mode(0o600);
        drop(options.open(&first)?);
        std::fs::hard_link(&first, root.join("state/identity-alias"))?;
        assert!(prepare_private_storage(&root).is_err());

        let special_root = temporary.path().canonicalize()?.join("special/tor");
        prepare_private_storage(&special_root)?;
        let socket_path = special_root.join("state/not-identity.sock");
        let _socket = UnixListener::bind(&socket_path)?;
        assert!(prepare_private_storage(&special_root).is_err());

        let permissive = temporary.path().canonicalize()?.join("permissive");
        std::fs::create_dir(&permissive)?;
        let mut permissions = std::fs::metadata(&permissive)?.permissions();
        permissions.set_mode(0o777);
        std::fs::set_permissions(&permissive, permissions)?;
        // `strict_mistrust` is constructed with `ignore_environment` and no
        // trusted group; there is no environment/config switch that can make
        // this unsafe directory pass.
        assert!(strict_mistrust()?.check_directory(&permissive).is_err());
        Ok(())
    }

    #[test]
    fn storage_repairs_only_the_fixed_arti_lock_files() -> Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?.join("private/tor");
        let storage = prepare_private_storage(&root)?;
        let locks = arti_lock_paths(&storage.state, &storage.cache);
        let identity = storage.state.join("identity-material");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).mode(0o600);
        std::io::Write::write_all(&mut options.open(&identity)?, b"identity remains unchanged")?;
        let identity_inode = std::fs::metadata(&identity)?.ino();

        for lock in &locks {
            let mut lock_permissions = std::fs::metadata(lock)?.permissions();
            lock_permissions.set_mode(0o4600);
            std::fs::set_permissions(lock, lock_permissions)?;
        }
        prepare_private_storage(&root)?;

        for lock in &locks {
            assert_eq!(std::fs::metadata(lock)?.mode() & 0o7777, 0o600);
        }
        assert_eq!(std::fs::metadata(&identity)?.ino(), identity_inode);
        assert_eq!(
            std::fs::read(&identity)?,
            b"identity remains unchanged".as_slice()
        );

        let unrelated = storage.state.join("unrelated-permissive-file");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).mode(0o644);
        drop(options.open(&unrelated)?);
        assert!(prepare_private_storage(&root).is_err());
        assert_eq!(std::fs::metadata(&unrelated)?.mode() & 0o777, 0o644);
        Ok(())
    }

    #[test]
    fn storage_lock_repair_rejects_link_redirection() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        for lock_index in 0..3 {
            for hard_link in [false, true] {
                let temporary = tempfile::tempdir()?;
                let root = temporary.path().canonicalize()?.join("private/tor");
                let storage = prepare_private_storage(&root)?;
                let lock = arti_lock_paths(&storage.state, &storage.cache)[lock_index].clone();
                std::fs::remove_file(&lock)?;

                let outside = temporary.path().join("outside-lock-target");
                let mut options = OpenOptions::new();
                options
                    .create_new(true)
                    .write(true)
                    .mode(0o644)
                    .truncate(false);
                std::io::Write::write_all(
                    &mut options.open(&outside)?,
                    b"outside remains unchanged",
                )?;
                if hard_link {
                    std::fs::hard_link(&outside, &lock)?;
                } else {
                    symlink(&outside, &lock)?;
                }

                assert!(prepare_private_storage(&root).is_err());
                assert_eq!(
                    std::fs::read(&outside)?,
                    b"outside remains unchanged".as_slice()
                );
                assert_eq!(
                    std::fs::metadata(&outside)?.permissions().mode() & 0o777,
                    0o644
                );
            }
        }
        Ok(())
    }

    #[test]
    #[expect(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "the lowercase .onion suffix is part of the canonical hostname, not a filesystem extension"
    )]
    fn actual_arti_identity_persists_across_reconstruction() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?.join("private/tor");
        let first = generate_identity(&root)?;
        let second = generate_identity(&root)?;
        assert_eq!(first, second);
        assert_eq!(first.len(), 62);
        assert!(first.ends_with(".onion"));
        Ok(())
    }

    fn generate_identity(root: &Path) -> Result<String> {
        let storage = prepare_private_storage(root)?;
        let config = build_arti_config(&storage).map_err(|error| anyhow::anyhow!(error.message))?;
        let nickname = HsNickname::new(SERVICE_NICKNAME.to_owned())?;
        let onion_config = OnionServiceConfigBuilder::default()
            .nickname(nickname)
            .build()?;
        let service = TorClient::<PreferredRuntime>::create_onion_service(&config, onion_config)?;
        let identity = service.generate_identity_key(KeystoreSelector::Primary)?;
        let hostname = identity.display_unredacted().to_string();
        Ok(hostname)
    }

    #[tokio::test]
    async fn enable_disable_readiness_network_recovery_and_identity_are_stable() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, false, false)?;
        let factory = FakeAttemptFactory::new(0);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        let target = loopback_target()?;
        assert_eq!(manager.status(), TorStatus::Disabled);

        manager.set_enabled(true).await?;
        let waiting = wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        assert_eq!(waiting.onion_host(), Some(TEST_ONION_HOST));

        manager.set_electrs_state(target, true).await?;
        let available = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(available.onion_host(), Some(TEST_ONION_HOST));

        manager.set_electrs_state(target, false).await?;
        let waiting_again = wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        assert_eq!(waiting_again.onion_host(), Some(TEST_ONION_HOST));
        assert_eq!(factory.starts(), 1, "electrs restart relaunched Arti");

        manager.set_electrs_state(target, true).await?;
        factory.send(FakeAction::Unavailable("scripted network loss"))?;
        let unavailable = wait_for_status(&manager, |status| {
            matches!(status, TorStatus::TemporarilyUnavailable { .. })
        })
        .await?;
        assert_eq!(unavailable.onion_host(), Some(TEST_ONION_HOST));
        factory.send(FakeAction::Reachable)?;
        let recovered = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(recovered.onion_host(), Some(TEST_ONION_HOST));
        assert_eq!(factory.starts(), 1, "network recovery relaunched Arti");

        manager.set_enabled(false).await?;
        assert_eq!(manager.status(), TorStatus::Disabled);
        assert_eq!(
            factory.stops(),
            1,
            "disable acknowledgement preceded active-attempt teardown"
        );
        manager.set_enabled(true).await?;
        wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        manager.set_electrs_state(target, true).await?;
        let restarted = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(restarted.onion_host(), Some(TEST_ONION_HOST));
        assert_eq!(factory.starts(), 2);

        manager.shutdown().await?;
        wait_for_count(&factory.stops, 2).await?;
        Ok(())
    }

    #[tokio::test]
    async fn readiness_and_target_reconfiguration_keep_the_running_onion_identity() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, true)?;
        let factory = FakeAttemptFactory::new(0);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        let before = wait_for_status(&manager, TorStatus::is_available).await?;
        let target = ElectrsTorTarget::new("192.168.50.7:50001".parse()?)?;
        manager.set_electrs_state(target, true).await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if factory.proxy_states().is_ok_and(|states| {
                    states
                        .iter()
                        .any(|state| state.target == target && state.ready)
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("target update did not reach the running proxy")?;
        assert_eq!(factory.starts(), 1, "target update relaunched Arti");
        assert_eq!(manager.status().onion_host(), before.onion_host());

        manager.set_electrs_state(target, false).await?;
        assert!(matches!(
            manager.status(),
            TorStatus::WaitingForElectrs { .. }
        ));
        let states = factory.proxy_states()?;
        assert!(states.windows(2).any(|states| {
            states[0].target == target
                && states[0].ready
                && states[1].target == target
                && !states[1].ready
        }));
        assert_eq!(factory.starts(), 1, "readiness loss relaunched Arti");
        assert_eq!(manager.status().onion_host(), before.onion_host());
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn unapplied_ready_update_retries_fail_closed_until_fresh_snapshot() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, false)?;
        let factory = FakeAttemptFactory::new(0);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        let initial = wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        let target = loopback_target()?;

        factory.fail_next_proxy_update();
        assert!(manager.set_electrs_state(target, true).await.is_err());
        wait_for_count(&factory.starts, 2).await?;
        let rejected_after_retry = wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        assert_eq!(rejected_after_retry.onion_host(), initial.onion_host());
        let proxy_states = factory.proxy_states()?;
        assert_eq!(proxy_states.last().map(|state| state.ready), Some(false));

        manager.set_electrs_state(target, true).await?;
        let available = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(available.onion_host(), initial.onion_host());
        assert_eq!(factory.starts(), 2, "fresh readiness relaunched Arti");
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn startup_failure_retries_automatically_and_manual_retry_is_immediate() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, true)?;
        let factory = FakeAttemptFactory::new(1);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        let failure = wait_for_status(&manager, |status| {
            matches!(
                status,
                TorStatus::TemporarilyUnavailable {
                    retry_in: Some(_),
                    ..
                }
            )
        })
        .await?;
        assert!(failure.onion_host().is_none());
        wait_for_count(&factory.starts, 2).await?;
        wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        manager.set_electrs_state(loopback_target()?, true).await?;
        let recovered = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(recovered.onion_host(), Some(TEST_ONION_HOST));
        assert_eq!(factory.starts(), 2);

        factory.send(FakeAction::Fail("scripted publication failure"))?;
        wait_for_status(&manager, |status| {
            matches!(status, TorStatus::TemporarilyUnavailable { .. })
        })
        .await?;
        manager.retry().await?;
        wait_for_count(&factory.starts, 3).await?;
        wait_for_status(&manager, |status| {
            matches!(status, TorStatus::WaitingForElectrs { .. })
        })
        .await?;
        manager.set_electrs_state(loopback_target()?, true).await?;
        let manually_recovered = wait_for_status(&manager, TorStatus::is_available).await?;
        assert_eq!(manually_recovered.onion_host(), Some(TEST_ONION_HOST));
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dropping_a_temporary_manager_clone_does_not_stop_the_worker() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, true)?;
        let factory = FakeAttemptFactory::new(0);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        wait_for_status(&manager, TorStatus::is_available).await?;
        let temporary_clone = manager.clone();
        drop(temporary_clone);
        tokio::task::yield_now().await;
        assert_eq!(factory.starts(), 1);
        assert_eq!(factory.stops(), 0);
        assert!(manager.status().is_available());
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dropping_the_final_manager_handle_stops_the_worker() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = test_manager_config(&temporary, true, true)?;
        let factory = FakeAttemptFactory::new(0);
        let manager = TorManager::spawn_with_factory(config, factory.clone(), test_retry_policy())?;
        wait_for_status(&manager, TorStatus::is_available).await?;
        drop(manager);
        wait_for_count(&factory.stops, 1).await?;
        Ok(())
    }

    #[test]
    fn bootstrap_progress_and_diagnostics_are_bounded() {
        assert_eq!(bootstrap_percent(f32::NAN), 0);
        assert_eq!(bootstrap_percent(-1.0), 0);
        assert_eq!(bootstrap_percent(0.734), 73);
        assert_eq!(bootstrap_percent(2.0), 100);
        let long = "x".repeat(MAX_DIAGNOSTIC_CHARS + 200);
        assert_eq!(
            bounded_diagnostic(long).chars().count(),
            MAX_DIAGNOSTIC_CHARS + 1
        );
    }

    #[test]
    fn ipv6_ula_detection_is_narrow() -> Result<()> {
        let allowed = SocketAddr::V6(SocketAddrV6::new(
            "fd00::1".parse()?,
            ELECTRUM_ONION_PORT,
            0,
            0,
        ));
        assert!(ElectrsTorTarget::new(allowed).is_ok());
        Ok(())
    }
}
