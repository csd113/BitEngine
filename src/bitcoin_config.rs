//! Resolution of the managed Bitcoin Core endpoints used by `BitEngine`.
//!
//! Bitcoin Core's P2P listener remains user-owned. `BitEngine` inspects the
//! effective mainnet configuration so it can connect Electrs to an existing
//! listener without adding or changing `port`, `bind`, or `listen` arguments.

use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Read as _,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context as _, Result};
use serde::{
    de::{Error as _, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::{Map, Value};

const DEFAULT_RPC_PORT: u16 = 8332;
const DEFAULT_P2P_PORT: u16 = 8333;
const DEFAULT_MAX_CONNECTIONS: i64 = 125;
const MINIMUM_ELECTRS_CONNECTIONS: i64 = 12;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_INCLUDED_CONFIGS: usize = 32;

const RELEVANT_OPTIONS: &[&str] = &[
    "bind",
    "connect",
    "includeconf",
    "listen",
    "maxconnections",
    "port",
    "proxy",
    "rpcport",
    "settings",
    "whitebind",
];

/// Endpoints and authentication path fixed for one managed Bitcoin generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedBitcoinEndpoints {
    pub rpc_candidates: Vec<SocketAddr>,
    pub rpc_port: u16,
    pub p2p_candidates: Vec<SocketAddr>,
    pub cookie_file: PathBuf,
}

/// Resolve the endpoints that the managed Core process will expose to `BitEngine`.
///
/// The caller must retain the result for the complete managed generation. Core
/// reads these files only during startup, so reparsing them for later status
/// polls could connect Electrs to a different endpoint after an external edit.
///
/// # Errors
///
/// Returns an error when relevant configuration cannot be inspected exactly,
/// contains an invalid endpoint, or disables the P2P service Electrs requires.
pub fn resolve_managed_endpoints(data_dir: &Path) -> Result<ManagedBitcoinEndpoints> {
    let data_dir = std::fs::canonicalize(data_dir)
        .with_context(|| format!("canonicalize Bitcoin data directory {}", data_dir.display()))?;
    let config = load_config(&data_dir)?;
    let settings = load_effective_settings(&data_dir, &config)?;

    let rpc_port = effective_scalar(&settings, &config, "rpcport")?
        .map_or(Ok(DEFAULT_RPC_PORT), |value| parse_port("rpcport", &value))?;
    let p2p_port = effective_scalar(&settings, &config, "port")?
        .map_or(Ok(DEFAULT_P2P_PORT), |value| parse_port("port", &value))?;

    let binds = effective_list(&settings, &config, "bind")?;
    let whitebinds = effective_list(&settings, &config, "whitebind")?;
    validate_listening(&settings, &config, &binds, &whitebinds)?;
    let p2p_candidates = resolve_p2p_candidates(&binds, &whitebinds, p2p_port)?;

    Ok(ManagedBitcoinEndpoints {
        rpc_candidates: vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, rpc_port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, rpc_port)),
        ],
        rpc_port,
        p2p_candidates,
        cookie_file: data_dir.join(".cookie"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawValue {
    Text(String),
    False,
    True,
}

impl RawValue {
    fn argument_text(&self) -> &str {
        match self {
            Self::Text(value) => value,
            Self::False => "0",
            Self::True => "1",
        }
    }
}

#[derive(Default)]
struct ConfigValues {
    global: HashMap<String, Vec<RawValue>>,
    main: HashMap<String, Vec<RawValue>>,
}

impl ConfigValues {
    fn push(&mut self, section: &str, name: &str, value: RawValue) {
        let target = match section {
            "" => &mut self.global,
            "main" => &mut self.main,
            _ => return,
        };
        target.entry(name.to_owned()).or_default().push(value);
    }

    fn section_values(&self, main: bool, name: &str) -> &[RawValue] {
        let source = if main { &self.main } else { &self.global };
        source.get(name).map_or(&[], Vec::as_slice)
    }

    fn scalar(&self, name: &str) -> Option<RawValue> {
        config_source_scalar(self.section_values(true, name))
            .or_else(|| config_source_scalar(self.section_values(false, name)))
            .cloned()
    }
}

fn load_config(data_dir: &Path) -> Result<ConfigValues> {
    let primary_path = data_dir.join("bitcoin.conf");
    let primary = read_bounded_regular(
        &primary_path,
        MAX_CONFIG_BYTES,
        MissingPolicy::Reject,
        "Bitcoin configuration",
    )?
    .context("Bitcoin configuration unexpectedly missing")?;

    let mut config = ConfigValues::default();
    parse_config(&primary, &primary_path, &mut config)?;

    // Core evaluates mainnet include directives before global directives,
    // independently of their textual order in the primary file.
    let mut includes = config_source_list(config.section_values(true, "includeconf"));
    includes.extend(config_source_list(
        config.section_values(false, "includeconf"),
    ));
    if includes.len() > MAX_INCLUDED_CONFIGS {
        bail!(
            "Bitcoin configuration contains {} includeconf entries; BitEngine can inspect at most {MAX_INCLUDED_CONFIGS}",
            includes.len()
        );
    }

    for include in includes {
        let raw_path = include.argument_text();
        let include_path = if Path::new(raw_path).is_absolute() {
            PathBuf::from(raw_path)
        } else {
            data_dir.join(raw_path)
        };
        let contents = read_bounded_regular(
            &include_path,
            MAX_CONFIG_BYTES,
            MissingPolicy::Reject,
            "included Bitcoin configuration",
        )?
        .with_context(|| {
            format!(
                "included Bitcoin configuration is missing: {}",
                include_path.display()
            )
        })?;
        parse_config(&contents, &include_path, &mut config)?;
    }

    Ok(config)
}

fn parse_config(text: &str, path: &Path, config: &mut ConfigValues) -> Result<()> {
    let mut current_section = String::new();

    for (line_index, raw_line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let line = raw_line
            .split_once('#')
            .map_or(raw_line, |(value, _)| value)
            .trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            line[1..line.len() - 1].clone_into(&mut current_section);
            continue;
        }
        if line.starts_with('-') {
            bail!(
                "invalid Bitcoin configuration {}:{line_number}: options must not start with '-'",
                path.display()
            );
        }

        let Some((raw_name, raw_value)) = line.split_once('=') else {
            bail!(
                "invalid Bitcoin configuration {}:{line_number}: expected name=value",
                path.display()
            );
        };
        let raw_name = raw_name.trim();
        if raw_name.is_empty() {
            bail!(
                "invalid Bitcoin configuration {}:{line_number}: option name is empty",
                path.display()
            );
        }
        let qualified_name = if current_section.is_empty() {
            raw_name.to_owned()
        } else {
            format!("{current_section}.{raw_name}")
        };
        let (section, name) = qualified_name
            .split_once('.')
            .map_or(("", qualified_name.as_str()), |(section, name)| {
                (section, name)
            });
        let (name, value) = interpret_config_value(name, raw_value.trim());
        if RELEVANT_OPTIONS.contains(&name) {
            config.push(section, name, value);
        }
    }

    Ok(())
}

fn interpret_config_value<'a>(name: &'a str, value: &str) -> (&'a str, RawValue) {
    name.strip_prefix("no").map_or_else(
        || (name, RawValue::Text(value.to_owned())),
        |positive_name| {
            (
                positive_name,
                if interpret_bool_text(value) {
                    RawValue::False
                } else {
                    RawValue::True
                },
            )
        },
    )
}

fn config_source_scalar(values: &[RawValue]) -> Option<&RawValue> {
    let start = values
        .iter()
        .rposition(|value| matches!(value, RawValue::False))
        .map_or(0, |index| index + 1);
    values.get(start).or_else(|| values.last())
}

fn config_source_list(values: &[RawValue]) -> Vec<RawValue> {
    let start = values
        .iter()
        .rposition(|value| matches!(value, RawValue::False))
        .map_or(0, |index| index + 1);
    values.get(start..).unwrap_or_default().to_vec()
}

type SettingsMap = Map<String, Value>;

struct UniqueSettings(SettingsMap);

impl<'de> Deserialize<'de> for UniqueSettings {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueSettingsVisitor;

        impl<'de> Visitor<'de> for UniqueSettingsVisitor {
            type Value = UniqueSettings;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Bitcoin settings JSON object with unique keys")
            }

            fn visit_map<A>(self, mut access: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut settings = SettingsMap::new();
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    if settings.insert(key.clone(), value).is_some() {
                        return Err(A::Error::custom(format!(
                            "duplicate Bitcoin settings key {key:?}"
                        )));
                    }
                }
                Ok(UniqueSettings(settings))
            }
        }

        deserializer.deserialize_map(UniqueSettingsVisitor)
    }
}

fn load_effective_settings(data_dir: &Path, config: &ConfigValues) -> Result<SettingsMap> {
    let configured = config.scalar("settings");
    let (settings_path, explicitly_selected) = match configured {
        Some(RawValue::False) => return Ok(SettingsMap::new()),
        Some(value) if !value.argument_text().is_empty() => {
            let configured_path = Path::new(value.argument_text());
            let path = if configured_path.is_absolute() {
                configured_path.to_path_buf()
            } else {
                data_dir.join(configured_path)
            };
            (path, true)
        }
        Some(_) | None => (data_dir.join("settings.json"), false),
    };

    let contents = read_bounded_regular(
        &settings_path,
        MAX_SETTINGS_BYTES,
        if explicitly_selected {
            MissingPolicy::Reject
        } else {
            MissingPolicy::Allow
        },
        "Bitcoin settings",
    )?;
    let Some(contents) = contents else {
        return Ok(SettingsMap::new());
    };
    let mut deserializer = serde_json::Deserializer::from_str(&contents);
    let UniqueSettings(settings) = UniqueSettings::deserialize(&mut deserializer)
        .with_context(|| format!("parse Bitcoin settings {}", settings_path.display()))?;
    deserializer
        .end()
        .with_context(|| format!("parse Bitcoin settings {}", settings_path.display()))?;
    Ok(settings)
}

fn effective_scalar(
    settings: &SettingsMap,
    config: &ConfigValues,
    name: &str,
) -> Result<Option<RawValue>> {
    if let Some(value) = settings.get(name) {
        return settings_scalar(name, value);
    }
    Ok(config.scalar(name))
}

fn settings_scalar(name: &str, value: &Value) -> Result<Option<RawValue>> {
    match value {
        Value::String(value) => Ok(Some(RawValue::Text(value.clone()))),
        Value::Number(_) if name == "listen" => {
            bail!("Bitcoin settings value \"listen\" must be a boolean or string, not a number")
        }
        Value::Number(value) => Ok(Some(RawValue::Text(value.to_string()))),
        Value::Bool(false) => Ok(Some(RawValue::False)),
        Value::Bool(true) => Ok(Some(RawValue::True)),
        // Core uses an explicit higher-precedence null to reset a scalar to
        // its default instead of falling through to bitcoin.conf.
        Value::Null => Ok(None),
        Value::Array(_) | Value::Object(_) => {
            bail!("Bitcoin settings value {name:?} must be a scalar")
        }
    }
}

struct SettingsListSource {
    values: Vec<RawValue>,
    negated: bool,
}

fn settings_list_source(name: &str, value: &Value) -> Result<SettingsListSource> {
    if value == &Value::Bool(false) {
        return Ok(SettingsListSource {
            values: Vec::new(),
            negated: true,
        });
    }
    let values = match value {
        Value::Array(values) => values
            .iter()
            .map(|value| {
                settings_scalar(name, value)?.with_context(|| {
                    format!("Bitcoin settings list {name:?} must not contain null")
                })
            })
            .collect::<Result<Vec<_>>>()?,
        value => vec![settings_scalar(name, value)?
            .with_context(|| format!("Bitcoin settings list {name:?} must not be null"))?],
    };
    Ok(SettingsListSource {
        values,
        negated: false,
    })
}

#[derive(Default)]
struct ListMerge {
    values: Vec<RawValue>,
    done: bool,
    previous_negated_empty: bool,
}

impl ListMerge {
    fn add_settings(&mut self, source: SettingsListSource) {
        self.values.extend(source.values);
        if source.negated {
            self.done = true;
            self.previous_negated_empty = self.values.is_empty();
        }
    }

    fn add_config(&mut self, source: &[RawValue]) {
        let last_negation = source
            .iter()
            .rposition(|value| matches!(value, RawValue::False));
        let start = last_negation.map_or(0, |index| index + 1);
        let add_zombie_values = !self.previous_negated_empty;
        if !self.done || add_zombie_values {
            self.values
                .extend(source.get(start..).unwrap_or_default().iter().cloned());
        }
        self.done |= last_negation.is_some();
        self.previous_negated_empty |=
            source.last() == Some(&RawValue::False) && self.values.is_empty();
    }
}

fn effective_list(
    settings: &SettingsMap,
    config: &ConfigValues,
    name: &str,
) -> Result<Vec<RawValue>> {
    let mut merged = ListMerge::default();
    if let Some(value) = settings.get(name) {
        merged.add_settings(settings_list_source(name, value)?);
    }
    merged.add_config(config.section_values(true, name));
    merged.add_config(config.section_values(false, name));
    Ok(merged.values)
}

fn validate_listening(
    settings: &SettingsMap,
    config: &ConfigValues,
    binds: &[RawValue],
    whitebinds: &[RawValue],
) -> Result<()> {
    let configured_listen = effective_scalar(settings, config, "listen")?
        .as_ref()
        .map(parse_bool);
    let has_explicit_bind = !binds.is_empty() || !whitebinds.is_empty();
    if configured_listen == Some(false) && has_explicit_bind {
        bail!(
            "Bitcoin Core setting listen=0 conflicts with bind/whitebind; remove the bind settings or set listen=1 before using managed Electrs"
        );
    }

    let connect_values = effective_list(settings, config, "connect")?;
    let connect_disables_listening =
        !connect_values.is_empty() || effective_value_is_false(settings, config, "connect");
    let proxy_active = effective_scalar(settings, config, "proxy")?.is_some_and(|value| {
        let value = value.argument_text();
        !value.is_empty() && value != "0"
    });
    let max_connections = effective_scalar(settings, config, "maxconnections")?
        .map_or(Ok(DEFAULT_MAX_CONNECTIONS), |value| {
            parse_integer("maxconnections", &value)
        })?;
    if max_connections < 0 {
        bail!("Bitcoin Core setting maxconnections must be non-negative");
    }

    let listening = configured_listen.unwrap_or(
        has_explicit_bind || !(connect_disables_listening || proxy_active || max_connections <= 0),
    );
    if !listening {
        let cause = if configured_listen == Some(false) {
            "listen=0"
        } else if connect_disables_listening {
            "connect/noconnect disables listening unless listen=1 or bind is explicit"
        } else if proxy_active {
            "proxy disables listening unless listen=1 or bind is explicit"
        } else {
            "maxconnections=0 disables listening unless listen=1 or bind is explicit"
        };
        bail!(
            "Bitcoin Core P2P listening is disabled ({cause}); managed Electrs requires a reachable P2P listener"
        );
    }
    if max_connections < MINIMUM_ELECTRS_CONNECTIONS {
        bail!(
            "Bitcoin Core setting maxconnections={max_connections} leaves no inbound slot for Electrs; set maxconnections to at least {MINIMUM_ELECTRS_CONNECTIONS}"
        );
    }

    Ok(())
}

fn effective_value_is_false(settings: &SettingsMap, config: &ConfigValues, name: &str) -> bool {
    settings.get(name).map_or_else(
        || matches!(config.scalar(name), Some(RawValue::False)),
        |value| value == &Value::Bool(false),
    )
}

fn parse_bool(value: &RawValue) -> bool {
    match value {
        RawValue::False => false,
        RawValue::True => true,
        RawValue::Text(value) => interpret_bool_text(value),
    }
}

fn interpret_bool_text(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }

    // Match Core's backwards-compatible `LocaleIndependentAtoi<int>` boolean
    // conversion: trim C-locale whitespace, accept one sign, and parse only
    // the leading decimal run. Saturation does not affect zero/nonzero.
    let value = value.trim_matches([' ', '\u{000c}', '\n', '\r', '\t', '\u{000b}']);
    let digits = value
        .strip_prefix('+')
        .filter(|value| !value.starts_with('-'))
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .any(|digit| digit != b'0')
}

fn parse_integer(name: &str, value: &RawValue) -> Result<i64> {
    value
        .argument_text()
        .parse::<i64>()
        .with_context(|| format!("Bitcoin Core setting {name} must be an integer"))
}

fn parse_port(name: &str, value: &RawValue) -> Result<u16> {
    let raw = value.argument_text();
    let port = raw
        .parse::<u16>()
        .with_context(|| format!("Bitcoin Core setting {name} must be a port from 1 to 65535"))?;
    if port == 0 {
        bail!("Bitcoin Core setting {name}=0 cannot provide a stable managed endpoint");
    }
    Ok(port)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateRank {
    V4Loopback,
    V4Wildcard,
    V6Loopback,
    V6Wildcard,
    WhitebindLoopback,
    OtherNormal,
    WhitebindOther,
    Onion,
}

struct Candidate {
    addr: SocketAddr,
    rank: CandidateRank,
}

fn resolve_p2p_candidates(
    binds: &[RawValue],
    whitebinds: &[RawValue],
    default_port: u16,
) -> Result<Vec<SocketAddr>> {
    let mut candidates = Vec::new();
    if binds.is_empty() && whitebinds.is_empty() {
        candidates.push(Candidate {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, default_port)),
            rank: CandidateRank::V4Wildcard,
        });
        candidates.push(Candidate {
            addr: SocketAddr::from((Ipv6Addr::LOCALHOST, default_port)),
            rank: CandidateRank::V6Wildcard,
        });
    } else {
        for value in binds {
            candidates.push(parse_bind(value.argument_text(), default_port)?);
        }
        for value in whitebinds {
            candidates.push(parse_whitebind(value.argument_text())?);
        }
    }

    candidates.sort_by_key(|candidate| candidate.rank);
    let mut result = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !result.contains(&candidate.addr) {
            result.push(candidate.addr);
        }
    }
    if result.is_empty() {
        bail!("Bitcoin Core configuration provides no P2P endpoint usable by managed Electrs");
    }
    Ok(result)
}

fn parse_bind(value: &str, default_port: u16) -> Result<Candidate> {
    let (address, onion) = if let Some((address, suffix)) = value.rsplit_once('=') {
        if suffix != "onion" {
            bail!("Bitcoin Core bind value {value:?} has unsupported suffix {suffix:?}");
        }
        (address, true)
    } else {
        (value, false)
    };
    let implicit_port = if onion {
        default_port.checked_add(1)
    } else {
        Some(default_port)
    };
    let socket = parse_socket_addr("bind", address, implicit_port)?;
    if onion {
        Ok(Candidate {
            addr: map_wildcard_to_loopback(socket),
            rank: CandidateRank::Onion,
        })
    } else {
        Ok(normal_candidate(socket))
    }
}

fn parse_whitebind(value: &str) -> Result<Candidate> {
    let address = if let Some((permissions, address)) = value.split_once('@') {
        validate_whitebind_permissions(permissions)?;
        address
    } else {
        value
    };
    let socket = parse_socket_addr("whitebind", address, None)?;
    let rank = if socket.ip().is_loopback() || socket.ip().is_unspecified() {
        CandidateRank::WhitebindLoopback
    } else {
        CandidateRank::WhitebindOther
    };
    Ok(Candidate {
        addr: map_wildcard_to_loopback(socket),
        rank,
    })
}

fn validate_whitebind_permissions(permissions: &str) -> Result<()> {
    let mut has_permission = false;
    let mut has_direction = false;
    for permission in permissions.split(',') {
        match permission {
            "" => {}
            "bloomfilter" | "bloom" | "noban" | "forcerelay" | "mempool" | "download" | "all"
            | "relay" | "addr" => has_permission = true,
            "in" => has_direction = true,
            "out" => {
                bail!("Bitcoin Core whitebind accepts incoming permissions only; remove 'out'")
            }
            _ => bail!("invalid Bitcoin Core whitebind permission {permission:?}"),
        }
    }
    if has_direction && !has_permission {
        bail!("Bitcoin Core whitebind specifies a direction but no permission");
    }
    Ok(())
}

fn parse_socket_addr(option: &str, value: &str, implicit_port: Option<u16>) -> Result<SocketAddr> {
    if let Ok(socket) = value.parse::<SocketAddr>() {
        if socket.port() == 0 {
            bail!("Bitcoin Core {option} value {value:?} uses port 0, which is not stable");
        }
        return Ok(socket);
    }

    let ip_text = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);
    let ip = ip_text.parse::<IpAddr>().with_context(|| {
        format!("Bitcoin Core {option} value {value:?} must contain a numeric IP address")
    })?;
    let port = implicit_port.with_context(|| {
        format!("Bitcoin Core {option} value {value:?} must include an explicit port")
    })?;
    if port == 0 {
        bail!("Bitcoin Core {option} value {value:?} resolves to unstable port 0");
    }
    Ok(SocketAddr::new(ip, port))
}

fn normal_candidate(socket: SocketAddr) -> Candidate {
    let (addr, rank) = match socket.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => (socket, CandidateRank::V4Loopback),
        IpAddr::V4(ip) if ip.is_unspecified() => (
            SocketAddr::from((Ipv4Addr::LOCALHOST, socket.port())),
            CandidateRank::V4Wildcard,
        ),
        IpAddr::V6(ip) if ip.is_loopback() => (socket, CandidateRank::V6Loopback),
        IpAddr::V6(ip) if ip.is_unspecified() => (
            SocketAddr::from((Ipv6Addr::LOCALHOST, socket.port())),
            CandidateRank::V6Wildcard,
        ),
        IpAddr::V4(_) | IpAddr::V6(_) => (socket, CandidateRank::OtherNormal),
    };
    Candidate { addr, rank }
}

fn map_wildcard_to_loopback(socket: SocketAddr) -> SocketAddr {
    match socket.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, socket.port()))
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, socket.port()))
        }
        IpAddr::V4(_) | IpAddr::V6(_) => socket,
    }
}

#[derive(Clone, Copy)]
enum MissingPolicy {
    Allow,
    Reject,
}

fn read_bounded_regular(
    path: &Path,
    limit: u64,
    missing: MissingPolicy,
    label: &str,
) -> Result<Option<String>> {
    let initial = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match missing {
                MissingPolicy::Allow => Ok(None),
                MissingPolicy::Reject => {
                    Err(error).with_context(|| format!("inspect {label} {}", path.display()))
                }
            };
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {label} {}", path.display()));
        }
    };
    if initial.file_type().is_symlink() || !initial.is_file() {
        bail!(
            "{label} must be a regular file and not a symlink: {}",
            path.display()
        );
    }
    if initial.len() > limit {
        bail!(
            "{label} exceeds the {limit}-byte inspection limit: {}",
            path.display()
        );
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect open {label} {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > limit {
        bail!("{label} changed while being inspected: {}", path.display());
    }

    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        bail!(
            "{label} exceeds the {limit}-byte inspection limit: {}",
            path.display()
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{label} is not UTF-8: {}", path.display()))?;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, fs, net::SocketAddr};

    use super::*;

    fn data_dir_with_config(config: &str) -> Result<tempfile::TempDir> {
        let temporary = tempfile::tempdir()?;
        fs::write(temporary.path().join("bitcoin.conf"), config)?;
        Ok(temporary)
    }

    fn addr(value: &str) -> Result<SocketAddr> {
        value.parse().map_err(anyhow::Error::from)
    }

    #[test]
    fn defaults_use_loopback_rpc_and_p2p_candidates() -> Result<()> {
        let data = data_dir_with_config("")?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, 8332);
        assert_eq!(
            endpoints.rpc_candidates,
            vec![addr("127.0.0.1:8332")?, addr("[::1]:8332")?]
        );
        assert_eq!(
            endpoints.p2p_candidates,
            vec![addr("127.0.0.1:8333")?, addr("[::1]:8333")?]
        );
        assert_eq!(
            endpoints.cookie_file,
            data.path().canonicalize()?.join(".cookie")
        );
        Ok(())
    }

    #[test]
    fn custom_ports_and_mainnet_scalar_precedence_are_honored() -> Result<()> {
        let data = data_dir_with_config(
            "rpcport=18332\nport=18333\n[main]\nrpcport=18443\nport=18444\n[test]\nrpcport=19332\nport=19333\n",
        )?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, 18_443);
        assert_eq!(endpoints.rpc_candidates[0], addr("127.0.0.1:18443")?);
        assert_eq!(endpoints.p2p_candidates[0], addr("127.0.0.1:18444")?);
        Ok(())
    }

    #[test]
    fn main_includes_are_loaded_before_global_includes() -> Result<()> {
        let data =
            data_dir_with_config("includeconf=global.conf\n[main]\nincludeconf=main.conf\n")?;
        fs::write(
            data.path().join("global.conf"),
            "rpcport=19001\nport=19002\n",
        )?;
        fs::write(data.path().join("main.conf"), "rpcport=18001\nport=18002\n")?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, 18_001);
        assert_eq!(endpoints.p2p_candidates[0], addr("127.0.0.1:18002")?);
        Ok(())
    }

    #[test]
    fn writable_settings_override_scalars_and_combine_repeatable_binds() -> Result<()> {
        let data = data_dir_with_config(
            "settings=state/custom.json\nport=18000\nbind=192.0.2.10:18000\n",
        )?;
        fs::create_dir(data.path().join("state"))?;
        fs::write(
            data.path().join("state/custom.json"),
            r#"{"rpcport":19001,"port":19002,"bind":["127.0.0.1:19003","[::]:19004"]}"#,
        )?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, 19_001);
        assert_eq!(
            endpoints.p2p_candidates,
            vec![
                addr("127.0.0.1:19003")?,
                addr("[::1]:19004")?,
                addr("192.0.2.10:18000")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn writable_null_scalars_reset_lower_config_values_to_defaults() -> Result<()> {
        let data = data_dir_with_config(
            "rpcport=19001\nport=19002\nlisten=0\nproxy=127.0.0.1:9050\nmaxconnections=1\n",
        )?;
        fs::write(
            data.path().join("settings.json"),
            r#"{"rpcport":null,"port":null,"listen":null,"proxy":null,"maxconnections":null}"#,
        )?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, DEFAULT_RPC_PORT);
        assert_eq!(
            endpoints.p2p_candidates,
            vec![addr("127.0.0.1:8333")?, addr("[::1]:8333")?,]
        );
        Ok(())
    }

    #[test]
    fn invalid_writable_boolean_and_repeatable_null_values_fail_closed() -> Result<()> {
        for settings in [
            r#"{"listen":1}"#,
            r#"{"bind":null}"#,
            r#"{"whitebind":null}"#,
            r#"{"connect":null}"#,
        ] {
            let data = data_dir_with_config("")?;
            fs::write(data.path().join("settings.json"), settings)?;
            assert!(
                resolve_managed_endpoints(data.path()).is_err(),
                "{settings}"
            );
        }
        Ok(())
    }

    #[test]
    fn legacy_boolean_text_matches_bitcoin_core_conversion() {
        for value in [
            "",
            "1",
            "-1",
            "+1",
            " 1 ",
            "1a",
            "1.9",
            "999999999999999999999",
        ] {
            assert!(interpret_bool_text(value), "{value:?}");
        }
        for value in [" ", "0", "-0", "abc", "+-1", "-+1", "0x1", "000a1"] {
            assert!(!interpret_bool_text(value), "{value:?}");
        }
    }

    #[test]
    fn bind_candidates_are_parsed_ranked_and_deduplicated() -> Result<()> {
        let data = data_dir_with_config(
            "port=19000\n\
             bind=192.0.2.10\n\
             bind=[::]\n\
             bind=0.0.0.0\n\
             bind=[::1]:19003\n\
             bind=127.0.0.1:19002\n\
             bind=127.0.0.1:19002\n\
             whitebind=noban@127.0.0.2:19004\n\
             bind=127.0.0.3=onion\n",
        )?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(
            endpoints.p2p_candidates,
            vec![
                addr("127.0.0.1:19002")?,
                addr("127.0.0.1:19000")?,
                addr("[::1]:19003")?,
                addr("[::1]:19000")?,
                addr("127.0.0.2:19004")?,
                addr("192.0.2.10:19000")?,
                addr("127.0.0.3:19001")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn config_negation_resets_earlier_repeatable_values() -> Result<()> {
        let data = data_dir_with_config("bind=192.0.2.10:18000\nnobind=1\nbind=127.0.0.1:19000\n")?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.p2p_candidates, vec![addr("127.0.0.1:19000")?]);
        Ok(())
    }

    #[test]
    fn mainnet_bind_values_precede_and_combine_with_global_values() -> Result<()> {
        let data = data_dir_with_config(
            "bind=192.0.2.10:18000\n\
             [main]\n\
             nobind=1\n\
             bind=[::1]:19000\n",
        )?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(
            endpoints.p2p_candidates,
            vec![addr("[::1]:19000")?, addr("192.0.2.10:18000")?]
        );
        Ok(())
    }

    #[test]
    fn explicit_bind_keeps_listening_enabled_with_proxy_or_connect() -> Result<()> {
        for extra in ["proxy=127.0.0.1:9050", "connect=example.invalid"] {
            let data = data_dir_with_config(&format!(
                "{extra}\nbind=127.0.0.1:19000\nmaxconnections=12\n"
            ))?;
            let endpoints = resolve_managed_endpoints(data.path())?;
            assert_eq!(endpoints.p2p_candidates[0], addr("127.0.0.1:19000")?);
        }
        Ok(())
    }

    #[test]
    fn disabled_or_capacityless_listening_is_rejected_with_the_cause() -> Result<()> {
        for (config, expected) in [
            ("listen=0\n", "listen=0"),
            ("connect=example.invalid\n", "connect/noconnect"),
            ("proxy=127.0.0.1:9050\n", "proxy disables"),
            ("maxconnections=0\n", "maxconnections=0"),
            ("listen=1\nmaxconnections=11\n", "leaves no inbound slot"),
        ] {
            let data = data_dir_with_config(config)?;
            let error = resolve_managed_endpoints(data.path())
                .expect_err("configuration should be incompatible");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        Ok(())
    }

    #[test]
    fn bind_combined_with_listen_zero_is_rejected_explicitly() -> Result<()> {
        let data = data_dir_with_config("listen=0\nbind=127.0.0.1:19000\n")?;

        let error =
            resolve_managed_endpoints(data.path()).expect_err("bind and listen=0 must conflict");

        assert!(error.to_string().contains("conflicts with bind/whitebind"));
        Ok(())
    }

    #[test]
    fn invalid_or_zero_managed_ports_are_rejected() -> Result<()> {
        for config in [
            "rpcport=0\n",
            "rpcport=invalid\n",
            "port=0\n",
            "port=70000\n",
        ] {
            let data = data_dir_with_config(config)?;
            assert!(resolve_managed_endpoints(data.path()).is_err());
        }
        Ok(())
    }

    #[test]
    fn nosettings_ignores_an_existing_settings_file() -> Result<()> {
        let data = data_dir_with_config("nosettings=1\nrpcport=19000\n")?;
        fs::write(data.path().join("settings.json"), "not json")?;

        let endpoints = resolve_managed_endpoints(data.path())?;

        assert_eq!(endpoints.rpc_port, 19_000);
        Ok(())
    }

    #[test]
    fn explicitly_selected_missing_settings_fail_closed() -> Result<()> {
        let data = data_dir_with_config("settings=missing.json\n")?;

        let error = resolve_managed_endpoints(data.path())
            .expect_err("explicit settings file must be inspectable");

        assert!(error.to_string().contains("Bitcoin settings"));
        Ok(())
    }

    #[test]
    fn missing_and_excess_includes_fail_closed() -> Result<()> {
        let missing = data_dir_with_config("includeconf=missing.conf\n")?;
        assert!(resolve_managed_endpoints(missing.path()).is_err());

        let mut config = String::new();
        for index in 0..=MAX_INCLUDED_CONFIGS {
            writeln!(&mut config, "includeconf={index}.conf")?;
        }
        let excess = data_dir_with_config(&config)?;
        let error = resolve_managed_endpoints(excess.path())
            .expect_err("include inspection limit must fail closed");
        assert!(error.to_string().contains("at most"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_include_fails_closed() -> Result<()> {
        use std::os::unix::fs::symlink;

        let data = data_dir_with_config("includeconf=alias.conf\n")?;
        fs::write(data.path().join("real.conf"), "port=19000\n")?;
        symlink(
            data.path().join("real.conf"),
            data.path().join("alias.conf"),
        )?;

        let error = resolve_managed_endpoints(data.path())
            .expect_err("symlinked include must not be followed");

        assert!(error.to_string().contains("not a symlink"));
        Ok(())
    }

    #[test]
    fn oversized_settings_fail_closed() -> Result<()> {
        let data = data_dir_with_config("")?;
        fs::write(
            data.path().join("settings.json"),
            vec![b' '; usize::try_from(MAX_SETTINGS_BYTES)? + 1],
        )?;

        let error = resolve_managed_endpoints(data.path())
            .expect_err("oversized settings must not be ignored");

        assert!(error.to_string().contains("inspection limit"));
        Ok(())
    }

    #[test]
    fn duplicate_settings_keys_fail_like_bitcoin_core() -> Result<()> {
        let data = data_dir_with_config("")?;
        fs::write(
            data.path().join("settings.json"),
            r#"{"port":19000,"port":19001}"#,
        )?;

        let error = resolve_managed_endpoints(data.path())
            .expect_err("Core rejects duplicate writable settings keys");

        assert!(format!("{error:#}").contains("duplicate Bitcoin settings key"));
        Ok(())
    }
}
