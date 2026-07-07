use std::{
    collections::BTreeMap,
    f32::consts::{FRAC_PI_2, PI},
    fs,
    io::BufRead,
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use chrono::Local;
use clap::Args;
use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
use motionstage_discovery::{DiscoveredService, DiscoveryBrowser};
use motionstage_protocol::{
    AttributeDescriptor, AttributeKind, ClientHello, ClientRole, ControlMessage, Feature, Mode,
    RecordingState, RegisterRequest, SamplingMode, SceneSnapshotPayload, StateEvent,
    StateEventEnvelope, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use motionstage_server::{ServerConfig, ServerHandle};
use motionstage_transport_quic::{
    AttributeUpdateFrame, AttributeValueFrame, ControlChannel, MotionDatagram, QuicClient, QuicPeer,
};
use tokio::{
    sync::mpsc,
    time::{interval, timeout, MissedTickBehavior},
};
use uuid::Uuid;

fn log_timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

macro_rules! sim_logln {
    ($($arg:tt)*) => {{
        println!("[{}] {}", log_timestamp(), format!($($arg)*));
    }};
}

#[derive(Debug, Clone, Args)]
pub struct SimulateArgs {
    /// Name advertised by the embedded server started by `simulate`.
    #[arg(long, default_value = "motionstage-sim")]
    pub name: String,
    /// Bind address for the embedded server (this is not a remote connect target).
    #[arg(
        long = "server-bind",
        visible_alias = "bind",
        default_value = "127.0.0.1:0"
    )]
    pub bind: SocketAddr,
    /// Connect target for client-only mode:
    /// `host:port`, `discover`, or `discover:<service-name>`.
    #[arg(long, value_parser = parse_connect_target)]
    pub connect: Option<ConnectTarget>,
    /// Discovery timeout in seconds when using `--connect discover...`.
    #[arg(long, default_value_t = 5)]
    pub connect_timeout_secs: u64,
    /// Source output attribute name sent in motion datagrams.
    #[arg(long, default_value = "demo.position")]
    pub output_attribute: String,
    /// Stable device id for mapping ownership; auto-generated if omitted.
    #[arg(long)]
    pub device_id: Option<Uuid>,
    /// Sample rate for generated motion updates.
    #[arg(long, default_value_t = 120)]
    pub sample_hz: u32,
    #[arg(long, default_value_t = 1.0)]
    pub amplitude: f32,
    #[arg(long, default_value_t = 0.5)]
    pub frequency_hz: f32,
    #[arg(long, default_value = "recordings/demo.cmtrk")]
    pub record_path: PathBuf,
    #[arg(long, default_value_t = 30)]
    pub print_every: u32,
    #[arg(long, default_value_t = false)]
    pub discoverable: bool,
    /// Print verbose simulator diagnostics (handshake/control/stream errors).
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}

#[derive(Debug, Clone)]
pub enum ConnectTarget {
    Address(SocketAddr),
    Discover(Option<String>),
}

#[derive(Debug, Clone)]
struct DemoRoute {
    source_device: Uuid,
    source_output: String,
    target: Option<DemoTarget>,
}

#[derive(Debug, Clone)]
struct DemoTarget {
    scene_id: Uuid,
    object_id: Uuid,
    attribute_name: String,
}

#[derive(Debug, Clone, Copy)]
enum SimulationMode {
    Embedded,
    ClientOnly,
}

#[derive(Debug, Clone)]
enum ResolvedConnectTarget {
    Direct(SocketAddr),
    Discovered {
        service_name: String,
        endpoint: SocketAddr,
        host_name: String,
        protocol_major: Option<u16>,
        protocol_minor: Option<u16>,
    },
}

impl ResolvedConnectTarget {
    fn endpoint(&self) -> SocketAddr {
        match self {
            Self::Direct(addr) => *addr,
            Self::Discovered { endpoint, .. } => *endpoint,
        }
    }
}

struct SimulatedClient {
    _endpoint: QuicClient,
    peer: QuicPeer,
    control: ControlChannel,
    session_id: Uuid,
}

#[derive(Debug, Clone)]
struct SimulatorState {
    streaming: bool,
    recording: bool,
    sample_index: u64,
    amplitude: f32,
    frequency_hz: f32,
    default_record_path: PathBuf,
    active_record_path: Option<PathBuf>,
    print_every: u32,
    last_error: Option<String>,
    selected_take: Option<Uuid>,
    playback_looping: bool,
    active_bake_cursor: Option<Uuid>,
    /// Take started over the wire (`take start`); identity is server-assigned.
    active_wire_take: Option<Uuid>,
}

impl SimulatorState {
    fn new(args: &SimulateArgs) -> Self {
        Self {
            streaming: false,
            recording: false,
            sample_index: 0,
            amplitude: args.amplitude,
            frequency_hz: args.frequency_hz,
            default_record_path: args.record_path.clone(),
            active_record_path: None,
            print_every: args.print_every,
            last_error: None,
            selected_take: None,
            playback_looping: false,
            active_bake_cursor: None,
            active_wire_take: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SimulationCommand {
    Help,
    Start,
    Stop,
    Status,
    RecordStart(Option<PathBuf>),
    RecordStop,
    TakesList,
    TakesSelect(Uuid),
    TakesDelete(Uuid),
    PlaybackPlay,
    PlaybackPause,
    PlaybackSeek(u64),
    PlaybackLoop(bool),
    PlaybackStop,
    BakeOpen(Uuid, SamplingMode),
    BakeNext,
    BakeSeek(u64),
    BakeClose,
    MappingCreate {
        object_id: Uuid,
        attribute: String,
        scene_id: Option<Uuid>,
    },
    MappingRemove(Uuid),
    TakeStart,
    TakeStop,
    Snapshot,
    Mode(Mode),
    Amp(f32),
    Freq(f32),
    Quit,
    Empty,
    Unknown(String),
}

pub async fn run(args: SimulateArgs) -> Result<()> {
    validate_args(&args)?;

    let source_device = args.device_id.unwrap_or_else(Uuid::now_v7);
    let source_output = qualify_source_output(source_device, args.output_attribute.trim());
    let connect_target = resolve_connect_target(&args).await?;

    let (mode, server, route, mut client) = match connect_target.as_ref() {
        Some(target) => {
            let connect_addr = target.endpoint();
            let (route, client) =
                bootstrap_remote_client(connect_addr, source_device, source_output, args.verbose)
                    .await?;
            (SimulationMode::ClientOnly, None, route, client)
        }
        None => {
            let config = ServerConfig {
                name: args.name.clone(),
                quic_bind_addr: args.bind,
                enable_discovery: args.discoverable,
                ..ServerConfig::default()
            };

            let server = ServerHandle::new(config);
            let advertisement = server.start().await.map_err(|err| {
                anyhow!(
                    "failed to start embedded simulator server at {}: {err}\n\
                     `simulate` starts its own server; `--server-bind` is the embedded server bind address.",
                    args.bind
                )
            })?;
            let server_addr = server.quic_bind_addr().await;
            let (route, client) = bootstrap_embedded_simulation(
                &server,
                server_addr,
                source_device,
                source_output,
                args.verbose,
            )
            .await?;
            print_embedded_banner(
                &advertisement.bind_host,
                advertisement.bind_port,
                args.bind,
                &route,
                &client,
            );
            (SimulationMode::Embedded, Some(server), route, client)
        }
    };
    let mut state = SimulatorState::new(&args);

    if let Some(target) = connect_target.as_ref() {
        print_client_only_banner(target, &route, &client);
    }

    print_help(mode);
    if args.verbose {
        sim_logln!(
            "debug: verbose diagnostics enabled (sample_hz={}, print_every={}, output_attribute={}, source_device={})",
            args.sample_hz, args.print_every, route.source_output, route.source_device
        );
    }
    print_prompt();

    let mut lines = spawn_stdin_reader();
    let tick_ns = (1_000_000_000_u64 / args.sample_hz as u64).max(1);
    let mut sample_interval = interval(Duration::from_nanos(tick_ns));
    sample_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut control_interval = interval(Duration::from_millis(250));
    control_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_control_ping_at = Instant::now();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = sample_interval.tick() => {
                if state.streaming {
                    if let Err(err) = emit_sample(
                        &client,
                        &route,
                        &mut state,
                        args.sample_hz,
                    ).await {
                        let message = if args.verbose {
                            format!("stream error: {err:#}")
                        } else {
                            format!("stream error: {err}")
                        };
                        sim_logln!("{message}");
                        state.last_error = Some(message);
                        state.streaming = false;
                        if args.verbose {
                            sim_logln!("note: streaming was stopped due to stream error");
                        }
                    }
                }
            }
            _ = control_interval.tick() => {
                if last_control_ping_at.elapsed() >= Duration::from_secs(2) {
                    if let Err(err) = client.control.send(&ControlMessage::Ping).await {
                        let message = format!("control heartbeat send failed: {err:#}");
                        sim_logln!("{message}");
                        state.last_error = Some(message);
                        state.streaming = false;
                    } else if args.verbose {
                        sim_logln!("debug: sent ControlMessage::Ping");
                    }
                    last_control_ping_at = Instant::now();
                }

                if let Err(err) = drain_control_messages(&mut client, &mut state, args.verbose).await {
                    let message = format!("control channel error: {err:#}");
                    sim_logln!("{message}");
                    state.last_error = Some(message);
                    state.streaming = false;
                }
            }
            line = lines.recv() => {
                let Some(line) = line else {
                    break;
                };
                let command = parse_command(&line);
                let should_continue = match handle_command(
                    server.as_ref(),
                    mode,
                    &mut client,
                    &route,
                    &mut state,
                    command,
                    args.verbose,
                )
                .await
                {
                    Ok(should_continue) => should_continue,
                    Err(err) => {
                        let message = if args.verbose {
                            format!("command error: {err:#}")
                        } else {
                            format!("command error: {err}")
                        };
                        sim_logln!("{message}");
                        state.last_error = Some(message);
                        true
                    }
                };
                if !should_continue {
                    break;
                }
                print_prompt();
            }
            _ = &mut ctrl_c => {
                sim_logln!("received ctrl+c, shutting down simulator");
                break;
            }
        }
    }

    if state.recording {
        if let Some(server) = server.as_ref() {
            let manifest = server.stop_recording().await?;
            sim_logln!(
                "recording stopped (id={}, frames={})",
                manifest.recording_id,
                manifest.frame_count
            );
        }
    }
    if state.streaming {
        if let Some(server) = server.as_ref() {
            server.set_mode(Mode::IDLE).await?;
        }
    }

    drop(client);
    if let Some(server) = server.as_ref() {
        server.stop().await?;
    }
    Ok(())
}

fn validate_args(args: &SimulateArgs) -> Result<()> {
    if args.sample_hz == 0 {
        return Err(anyhow!("sample_hz must be greater than zero"));
    }
    if !args.amplitude.is_finite() || args.amplitude < 0.0 {
        return Err(anyhow!("amplitude must be a finite number >= 0"));
    }
    if !args.frequency_hz.is_finite() || args.frequency_hz < 0.0 {
        return Err(anyhow!("frequency_hz must be a finite number >= 0"));
    }
    if args.print_every == 0 {
        return Err(anyhow!("print_every must be greater than zero"));
    }
    if matches!(args.connect, Some(ConnectTarget::Discover(_))) && args.connect_timeout_secs == 0 {
        return Err(anyhow!(
            "connect_timeout_secs must be greater than zero for discovery"
        ));
    }
    if args.output_attribute.trim().is_empty() {
        return Err(anyhow!("output_attribute must not be empty"));
    }
    Ok(())
}

fn qualify_source_output(device_id: Uuid, output_attribute: &str) -> String {
    let normalized = output_attribute.trim();
    if normalized.is_empty() {
        return normalized.to_owned();
    }

    let expected_prefix = format!("{device_id}.");
    if normalized.starts_with(&expected_prefix) {
        return normalized.to_owned();
    }

    let prefix = normalized.split('.').next().unwrap_or_default();
    if Uuid::parse_str(prefix).is_ok() {
        return normalized.to_owned();
    }

    format!("{device_id}.{normalized}")
}

fn parse_connect_target(value: &str) -> std::result::Result<ConnectTarget, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("discover") {
        return Ok(ConnectTarget::Discover(None));
    }
    if let Some(name) = trimmed.strip_prefix("discover:") {
        let service_name = name.trim();
        if service_name.is_empty() {
            return Err(
                "invalid `--connect` target: expected `discover:<service-name>`".to_owned(),
            );
        }
        return Ok(ConnectTarget::Discover(Some(service_name.to_owned())));
    }
    SocketAddr::from_str(trimmed)
        .map(ConnectTarget::Address)
        .map_err(|_| {
            format!(
                "invalid `--connect` target `{trimmed}`; expected `host:port`, `discover`, or `discover:<service-name>`"
            )
        })
}

async fn resolve_connect_target(args: &SimulateArgs) -> Result<Option<ResolvedConnectTarget>> {
    let Some(connect) = args.connect.clone() else {
        return Ok(None);
    };

    match connect {
        ConnectTarget::Address(addr) => Ok(Some(ResolvedConnectTarget::Direct(addr))),
        ConnectTarget::Discover(service_name) => {
            let timeout = Duration::from_secs(args.connect_timeout_secs);
            let resolved = tokio::task::spawn_blocking(move || {
                discover_connect_target(service_name.as_deref(), timeout)
            })
            .await
            .map_err(|err| anyhow!("discovery task failed: {err}"))??;
            Ok(Some(resolved))
        }
    }
}

fn discover_connect_target(
    service_name_filter: Option<&str>,
    timeout: Duration,
) -> Result<ResolvedConnectTarget> {
    let browser =
        DiscoveryBrowser::start().map_err(|err| anyhow!("failed to start mDNS browser: {err}"))?;
    let deadline = std::time::Instant::now() + timeout;
    let filter = service_name_filter.map(normalize_service_name);
    let mut candidates: BTreeMap<String, ResolvedConnectTarget> = BTreeMap::new();

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.duration_since(now);
        let event = browser
            .recv_service_timeout(remaining)
            .map_err(|err| anyhow!("discovery browse failed: {err}"))?;
        let Some(service) = event else {
            break;
        };

        let Some(endpoint) = select_preferred_endpoint(&service) else {
            continue;
        };
        let candidate = ResolvedConnectTarget::Discovered {
            service_name: service.service_name.clone(),
            endpoint,
            host_name: service.host_name,
            protocol_major: service.protocol_major,
            protocol_minor: service.protocol_minor,
        };

        if let Some(filter) = filter.as_deref() {
            if normalize_service_name(&service.service_name) == filter {
                let _ = browser.stop();
                return Ok(candidate);
            }
            continue;
        }

        candidates.insert(service.service_name, candidate);
    }

    let _ = browser.stop();
    if let Some(service_name) = service_name_filter {
        return Err(anyhow!(
            "no motionstage server named `{service_name}` discovered within {}s",
            timeout.as_secs()
        ));
    }

    match candidates.len() {
        0 => Err(anyhow!(
            "no motionstage servers discovered within {}s; use `--connect host:port` or ensure discovery is enabled",
            timeout.as_secs()
        )),
        1 => Ok(candidates.into_values().next().expect("single candidate exists")),
        _ => {
            let options = candidates
                .values()
                .map(|target| match target {
                    ResolvedConnectTarget::Direct(_) => String::new(),
                    ResolvedConnectTarget::Discovered {
                        service_name,
                        endpoint,
                        ..
                    } => format!("{service_name} ({endpoint})"),
                })
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!(
                "multiple motionstage servers discovered: {options}; rerun with `--connect discover:<service-name>` to select one"
            ))
        }
    }
}

fn normalize_service_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn select_preferred_endpoint(service: &DiscoveredService) -> Option<SocketAddr> {
    let mut addresses = service.addresses.clone();
    addresses.sort_by_key(|ip| {
        (
            if ip.is_ipv4() { 0 } else { 1 },
            if ip.is_loopback() { 1 } else { 0 },
            ip.to_string(),
        )
    });
    addresses
        .into_iter()
        .next()
        .map(|ip| SocketAddr::new(ip, service.port))
}

async fn bootstrap_embedded_simulation(
    server: &ServerHandle,
    server_addr: SocketAddr,
    source_device: Uuid,
    source_output: String,
    verbose: bool,
) -> Result<(DemoRoute, SimulatedClient)> {
    let attribute_name = "position".to_owned();

    let object = SceneObject::new("demo_camera").with_attribute(SceneAttribute::new(
        attribute_name.clone(),
        AttributeValue::Vec3f([0.0, 0.0, 0.0]),
    ));
    let object_id = object.id;
    let scene = Scene::new("demo_scene").with_object(object);
    let scene_id = scene.id;
    let loaded_scene = server.load_scene(scene).await;
    server.set_active_scene(loaded_scene).await?;

    let client =
        connect_simulated_client(server_addr, source_device, &source_output, verbose).await?;

    server
        .create_mapping(
            MappingRequest {
                source_device,
                source_output: source_output.clone(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: attribute_name.clone(),
                component_mask: None,
            },
            now_ns(),
        )
        .await?;

    Ok((
        DemoRoute {
            source_device,
            source_output,
            target: Some(DemoTarget {
                scene_id,
                object_id,
                attribute_name,
            }),
        },
        client,
    ))
}

async fn bootstrap_remote_client(
    connect_addr: SocketAddr,
    source_device: Uuid,
    source_output: String,
    verbose: bool,
) -> Result<(DemoRoute, SimulatedClient)> {
    let client =
        connect_simulated_client(connect_addr, source_device, &source_output, verbose).await?;

    Ok((
        DemoRoute {
            source_device,
            source_output,
            target: None,
        },
        client,
    ))
}

async fn emit_sample(
    client: &SimulatedClient,
    route: &DemoRoute,
    state: &mut SimulatorState,
    sample_hz: u32,
) -> Result<()> {
    let t = state.sample_index as f32 / sample_hz as f32;
    let value = sine_vec3(state.amplitude, state.frequency_hz, t);

    client.peer.send_motion_datagram(MotionDatagram {
        device_id: route.source_device,
        timestamp_ns: now_ns(),
        updates: vec![AttributeUpdateFrame {
            output_attribute: route.source_output.clone(),
            value: AttributeValueFrame::Vec3f(value),
        }],
    })?;

    if state.sample_index.is_multiple_of(state.print_every as u64) {
        sim_logln!(
            "sample {:>8} -> [{:>7.3}, {:>7.3}, {:>7.3}]",
            state.sample_index,
            value[0],
            value[1],
            value[2]
        );
    }

    state.sample_index += 1;
    Ok(())
}

async fn handle_command(
    server: Option<&ServerHandle>,
    mode: SimulationMode,
    client: &mut SimulatedClient,
    route: &DemoRoute,
    state: &mut SimulatorState,
    command: SimulationCommand,
    verbose: bool,
) -> Result<bool> {
    if verbose && !matches!(command, SimulationCommand::Empty) {
        sim_logln!("debug: command={command:?}");
    }
    match command {
        SimulationCommand::Help => {
            print_help(mode);
        }
        SimulationCommand::Start => {
            if let Some(server) = server {
                server.set_mode(Mode::LIVE).await?;
                sim_logln!("streaming started (mode=Live)");
            } else {
                let active_mode = request_remote_mode(client, Mode::LIVE, verbose).await?;
                sim_logln!("streaming started (client-only mode, remote mode={active_mode:?})");
            }
            state.streaming = true;
            state.last_error = None;
        }
        SimulationCommand::Stop => {
            if state.recording {
                sim_logln!("recording is active, run `record stop` first");
                return Ok(true);
            }
            state.streaming = false;
            if let Some(server) = server {
                server.set_mode(Mode::IDLE).await?;
                sim_logln!("streaming stopped (mode=Idle)");
            } else {
                let active_mode = request_remote_mode(client, Mode::IDLE, verbose).await?;
                sim_logln!("streaming stopped (client-only mode, remote mode={active_mode:?})");
            }
        }
        SimulationCommand::Status => {
            print_status(server, route, state).await?;
        }
        SimulationCommand::RecordStart(path) => {
            let Some(server) = server else {
                sim_logln!("record start is unavailable in client-only mode");
                return Ok(true);
            };
            if state.recording {
                sim_logln!("recording is already active");
                return Ok(true);
            }
            if !state.streaming {
                server.set_mode(Mode::LIVE).await?;
                state.streaming = true;
            }
            let path = path.unwrap_or_else(|| state.default_record_path.clone());
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            let recording_id = server.start_recording(&path, now_ns()).await?;
            state.recording = true;
            state.active_record_path = Some(path.clone());
            sim_logln!(
                "recording started (id={recording_id}, path={})",
                path.display()
            );
        }
        SimulationCommand::RecordStop => {
            let Some(server) = server else {
                sim_logln!("record stop is unavailable in client-only mode");
                return Ok(true);
            };
            if !state.recording {
                sim_logln!("no active recording");
                return Ok(true);
            }
            let manifest = server.stop_recording().await?;
            state.recording = false;
            let path = state
                .active_record_path
                .as_ref()
                .map(|v| v.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());
            sim_logln!(
                "recording stopped (id={}, frames={}, path={})",
                manifest.recording_id,
                manifest.frame_count,
                path
            );
            state.selected_take = Some(manifest.recording_id);
            sim_logln!("selected take set to {}", manifest.recording_id);
            state.active_record_path = None;
        }
        SimulationCommand::TakesList => {
            let Some(server) = server else {
                sim_logln!("takes list is unavailable in client-only mode");
                return Ok(true);
            };
            let takes = server.list_takes(None).await;
            if takes.is_empty() {
                sim_logln!("no takes available");
            } else {
                for take in takes {
                    sim_logln!(
                        "take {} scene={} name=\"{}\" frames={} selected={} path={}",
                        take.take_id,
                        take.scene_id,
                        take.name,
                        take.frame_count,
                        take.selected,
                        take.path
                    );
                    if take.selected {
                        state.selected_take = Some(take.take_id);
                    }
                }
            }
        }
        SimulationCommand::TakesSelect(take_id) => {
            let Some(server) = server else {
                sim_logln!("takes select is unavailable in client-only mode");
                return Ok(true);
            };
            let selected = server.select_take(take_id).await?;
            state.selected_take = Some(selected.take_id);
            sim_logln!("selected take {}", selected.take_id);
        }
        SimulationCommand::TakesDelete(take_id) => {
            let Some(server) = server else {
                sim_logln!("takes delete is unavailable in client-only mode");
                return Ok(true);
            };
            server.delete_take(take_id).await?;
            if state.selected_take == Some(take_id) {
                state.selected_take = None;
            }
            sim_logln!("deleted take {take_id}");
        }
        SimulationCommand::PlaybackPlay => {
            let Some(server) = server else {
                sim_logln!("playback is unavailable in client-only mode");
                return Ok(true);
            };
            let Some(take_id) = state.selected_take else {
                sim_logln!("no selected take; run `takes select <take_id>` first");
                return Ok(true);
            };
            let (_state, playhead_ns, looping) = server
                .playback_play(take_id, state.playback_looping)
                .await?;
            state.playback_looping = looping;
            sim_logln!(
                "playback started take={} playhead_ns={} looping={}",
                take_id,
                playhead_ns,
                looping
            );
        }
        SimulationCommand::PlaybackPause => {
            let Some(server) = server else {
                sim_logln!("playback is unavailable in client-only mode");
                return Ok(true);
            };
            let Some(take_id) = state.selected_take else {
                sim_logln!("no selected take; run `takes select <take_id>` first");
                return Ok(true);
            };
            let (_state, playhead_ns, looping) = server.playback_pause(take_id).await?;
            state.playback_looping = looping;
            sim_logln!(
                "playback paused take={} playhead_ns={} looping={}",
                take_id,
                playhead_ns,
                looping
            );
        }
        SimulationCommand::PlaybackSeek(seek_ns) => {
            let Some(server) = server else {
                sim_logln!("playback is unavailable in client-only mode");
                return Ok(true);
            };
            let Some(take_id) = state.selected_take else {
                sim_logln!("no selected take; run `takes select <take_id>` first");
                return Ok(true);
            };
            let (_state, playhead_ns, looping) = server
                .playback_seek(take_id, seek_ns, state.playback_looping)
                .await?;
            state.playback_looping = looping;
            sim_logln!(
                "playback seek take={} playhead_ns={} looping={}",
                take_id,
                playhead_ns,
                looping
            );
        }
        SimulationCommand::PlaybackLoop(looping) => {
            state.playback_looping = looping;
            sim_logln!("playback looping set to {}", state.playback_looping);
        }
        SimulationCommand::PlaybackStop => {
            let Some(server) = server else {
                sim_logln!("playback is unavailable in client-only mode");
                return Ok(true);
            };
            let Some(take_id) = state.selected_take else {
                sim_logln!("no selected take; run `takes select <take_id>` first");
                return Ok(true);
            };
            let (_state, playhead_ns, looping) = server.playback_stop(take_id).await?;
            state.playback_looping = looping;
            sim_logln!(
                "playback stopped take={} playhead_ns={} looping={}",
                take_id,
                playhead_ns,
                looping
            );
        }
        SimulationCommand::BakeOpen(take_id, sampling_mode) => {
            let Some(server) = server else {
                sim_logln!("bake commands are unavailable in client-only mode");
                return Ok(true);
            };
            let (cursor_id, total_frames) =
                server.open_take_bake_cursor(take_id, sampling_mode).await?;
            state.active_bake_cursor = Some(cursor_id);
            sim_logln!(
                "bake cursor opened id={} take={} total_frames={} sampling={:?}",
                cursor_id,
                take_id,
                total_frames,
                sampling_mode
            );
        }
        SimulationCommand::BakeNext => {
            let Some(server) = server else {
                sim_logln!("bake commands are unavailable in client-only mode");
                return Ok(true);
            };
            let Some(cursor_id) = state.active_bake_cursor else {
                sim_logln!("no active bake cursor; run `bake open <take_id>` first");
                return Ok(true);
            };
            match server.read_take_bake_frame(cursor_id).await? {
                Some((frame_index, timestamp_ns, attributes)) => {
                    sim_logln!(
                        "bake frame cursor={} index={} timestamp_ns={} attributes={}",
                        cursor_id,
                        frame_index,
                        timestamp_ns,
                        attributes.len()
                    );
                }
                None => sim_logln!("bake cursor reached end of stream"),
            }
        }
        SimulationCommand::BakeSeek(frame_index) => {
            let Some(server) = server else {
                sim_logln!("bake commands are unavailable in client-only mode");
                return Ok(true);
            };
            let Some(cursor_id) = state.active_bake_cursor else {
                sim_logln!("no active bake cursor; run `bake open <take_id>` first");
                return Ok(true);
            };
            match server.seek_take_bake_frame(cursor_id, frame_index).await? {
                Some((resolved_index, timestamp_ns, attributes)) => {
                    sim_logln!(
                        "bake seek cursor={} index={} timestamp_ns={} attributes={}",
                        cursor_id,
                        resolved_index,
                        timestamp_ns,
                        attributes.len()
                    );
                }
                None => sim_logln!("bake seek out of range"),
            }
        }
        SimulationCommand::BakeClose => {
            let Some(server) = server else {
                sim_logln!("bake commands are unavailable in client-only mode");
                return Ok(true);
            };
            let Some(cursor_id) = state.active_bake_cursor else {
                sim_logln!("no active bake cursor");
                return Ok(true);
            };
            server.close_take_bake_cursor(cursor_id).await?;
            state.active_bake_cursor = None;
            sim_logln!("bake cursor closed {cursor_id}");
        }
        SimulationCommand::MappingCreate {
            object_id,
            attribute,
            scene_id,
        } => {
            // Wire operator-plane op: works in both modes. `source_device:
            // None` = this session's own device; `target_scene: None` = the
            // active scene.
            client
                .control
                .send(&ControlMessage::CreateMapping {
                    source_device: None,
                    source_output: route.source_output.clone(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: attribute,
                    component_mask: None,
                })
                .await?;
            let reply = wait_for_wire_reply(client, "mapping create", verbose, |msg| {
                matches!(msg, ControlMessage::MappingCreateResult { .. })
            })
            .await?;
            if let ControlMessage::MappingCreateResult { result } = reply {
                match result {
                    Ok(mapping) => sim_logln!(
                        "mapping created {} ({} -> scene:{} object:{} attr:{})",
                        mapping.mapping_id,
                        mapping.source_output,
                        mapping.target_scene,
                        mapping.target_object,
                        mapping.target_attribute
                    ),
                    Err(err) => sim_logln!(
                        "mapping create rejected: code={:?} reason={}",
                        err.code,
                        err.reason
                    ),
                }
            }
        }
        SimulationCommand::MappingRemove(mapping_id) => {
            client
                .control
                .send(&ControlMessage::RemoveMapping { mapping_id })
                .await?;
            let reply = wait_for_wire_reply(client, "mapping remove", verbose, |msg| {
                matches!(
                    msg,
                    ControlMessage::MappingOpResult { mapping_id: id, .. } if *id == mapping_id
                )
            })
            .await?;
            if let ControlMessage::MappingOpResult { result, .. } = reply {
                match result {
                    Ok(()) => sim_logln!("mapping removed {mapping_id}"),
                    Err(err) => sim_logln!(
                        "mapping remove rejected: code={:?} reason={}",
                        err.code,
                        err.reason
                    ),
                }
            }
        }
        SimulationCommand::TakeStart => {
            // Wire take control (Operator-gated): take identity and the
            // recording path are server-assigned.
            client.control.send(&ControlMessage::StartTake).await?;
            let reply = wait_for_wire_reply(client, "take start", verbose, |msg| {
                matches!(msg, ControlMessage::TakeStartResult { .. })
            })
            .await?;
            if let ControlMessage::TakeStartResult { result } = reply {
                match result {
                    Ok(take_id) => {
                        state.active_wire_take = Some(take_id);
                        sim_logln!("take started (server-assigned id={take_id})");
                    }
                    Err(err) => sim_logln!(
                        "take start rejected: code={:?} reason={}",
                        err.code,
                        err.reason
                    ),
                }
            }
        }
        SimulationCommand::TakeStop => {
            client.control.send(&ControlMessage::StopTake).await?;
            let reply = wait_for_wire_reply(client, "take stop", verbose, |msg| {
                matches!(msg, ControlMessage::TakeStopResult { .. })
            })
            .await?;
            if let ControlMessage::TakeStopResult { result } = reply {
                match result {
                    Ok(take) => {
                        state.active_wire_take = None;
                        state.selected_take = Some(take.take_id);
                        sim_logln!(
                            "take stopped and registered (id={}, name=\"{}\", frames={}, path={})",
                            take.take_id,
                            take.name,
                            take.frame_count,
                            take.path
                        );
                    }
                    Err(err) => sim_logln!(
                        "take stop rejected: code={:?} reason={}",
                        err.code,
                        err.reason
                    ),
                }
            }
        }
        SimulationCommand::Snapshot => {
            client
                .control
                .send(&ControlMessage::GetSceneSnapshot)
                .await?;
            let reply = wait_for_wire_reply(client, "snapshot", verbose, |msg| {
                matches!(msg, ControlMessage::SceneSnapshot(_))
            })
            .await?;
            if let ControlMessage::SceneSnapshot(snapshot) = reply {
                log_scene_snapshot(&snapshot);
                print_snapshot_details(&snapshot);
            }
        }
        SimulationCommand::Amp(value) => {
            if value < 0.0 || !value.is_finite() {
                sim_logln!("amplitude must be a finite number >= 0");
            } else {
                state.amplitude = value;
                sim_logln!("amplitude set to {}", state.amplitude);
            }
        }
        SimulationCommand::Freq(value) => {
            if value < 0.0 || !value.is_finite() {
                sim_logln!("frequency must be a finite number >= 0");
            } else {
                state.frequency_hz = value;
                sim_logln!("frequency set to {}", state.frequency_hz);
            }
        }
        SimulationCommand::Mode(mode_request) => {
            if let Some(server) = server {
                server.set_mode(mode_request).await?;
                sim_logln!("mode set to {mode_request:?}");
            } else {
                let active_mode = request_remote_mode(client, mode_request, verbose).await?;
                sim_logln!("remote mode set to {active_mode:?}");
            }
        }
        SimulationCommand::Quit => {
            sim_logln!("exiting simulator");
            return Ok(false);
        }
        SimulationCommand::Empty => {}
        SimulationCommand::Unknown(value) => {
            sim_logln!("unknown command `{value}` (run `help`)");
        }
    }
    Ok(true)
}

fn parse_command(line: &str) -> SimulationCommand {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return SimulationCommand::Empty;
    }

    let mut parts = trimmed.split_whitespace();
    let Some(command) = parts.next() else {
        return SimulationCommand::Empty;
    };

    match command {
        "help" | "h" => SimulationCommand::Help,
        "start" => SimulationCommand::Start,
        "stop" => SimulationCommand::Stop,
        "status" => SimulationCommand::Status,
        "quit" | "exit" => SimulationCommand::Quit,
        "record" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "start" => {
                    let remainder = parts.collect::<Vec<_>>().join(" ");
                    if remainder.is_empty() {
                        SimulationCommand::RecordStart(None)
                    } else {
                        SimulationCommand::RecordStart(Some(PathBuf::from(remainder)))
                    }
                }
                "stop" => SimulationCommand::RecordStop,
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "takes" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "list" => SimulationCommand::TakesList,
                "select" => match parts.next().and_then(|raw| Uuid::parse_str(raw).ok()) {
                    Some(take_id) => SimulationCommand::TakesSelect(take_id),
                    None => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                "delete" => match parts.next().and_then(|raw| Uuid::parse_str(raw).ok()) {
                    Some(take_id) => SimulationCommand::TakesDelete(take_id),
                    None => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "playback" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "play" => SimulationCommand::PlaybackPlay,
                "pause" => SimulationCommand::PlaybackPause,
                "seek" => match parts.next().and_then(|raw| raw.parse::<u64>().ok()) {
                    Some(ns) => SimulationCommand::PlaybackSeek(ns),
                    None => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                "loop" => match parts.next() {
                    Some("on") => SimulationCommand::PlaybackLoop(true),
                    Some("off") => SimulationCommand::PlaybackLoop(false),
                    _ => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                "stop" => SimulationCommand::PlaybackStop,
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "bake" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "open" => {
                    let Some(take_raw) = parts.next() else {
                        return SimulationCommand::Unknown(trimmed.to_owned());
                    };
                    let Some(take_id) = Uuid::parse_str(take_raw).ok() else {
                        return SimulationCommand::Unknown(trimmed.to_owned());
                    };
                    let sampling_mode = parts
                        .next()
                        .map(parse_sampling_mode)
                        .unwrap_or(Some(SamplingMode::Captured));
                    match sampling_mode {
                        Some(mode) => SimulationCommand::BakeOpen(take_id, mode),
                        None => SimulationCommand::Unknown(trimmed.to_owned()),
                    }
                }
                "next" => SimulationCommand::BakeNext,
                "seek" => match parts.next().and_then(|raw| raw.parse::<u64>().ok()) {
                    Some(index) => SimulationCommand::BakeSeek(index),
                    None => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                "close" => SimulationCommand::BakeClose,
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "mapping" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "create" => {
                    let Some(object_id) = parts.next().and_then(|raw| Uuid::parse_str(raw).ok())
                    else {
                        return SimulationCommand::Unknown(trimmed.to_owned());
                    };
                    let Some(attribute) = parts.next().map(str::to_owned) else {
                        return SimulationCommand::Unknown(trimmed.to_owned());
                    };
                    let scene_id = match parts.next() {
                        Some(raw) => match Uuid::parse_str(raw) {
                            Ok(scene_id) => Some(scene_id),
                            Err(_) => return SimulationCommand::Unknown(trimmed.to_owned()),
                        },
                        None => None,
                    };
                    SimulationCommand::MappingCreate {
                        object_id,
                        attribute,
                        scene_id,
                    }
                }
                "remove" => match parts.next().and_then(|raw| Uuid::parse_str(raw).ok()) {
                    Some(mapping_id) => SimulationCommand::MappingRemove(mapping_id),
                    None => SimulationCommand::Unknown(trimmed.to_owned()),
                },
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "take" => {
            let Some(op) = parts.next() else {
                return SimulationCommand::Unknown(trimmed.to_owned());
            };
            match op {
                "start" => SimulationCommand::TakeStart,
                "stop" => SimulationCommand::TakeStop,
                _ => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        "snapshot" => SimulationCommand::Snapshot,
        "amp" => match parts.next().and_then(|v| v.parse::<f32>().ok()) {
            Some(v) => SimulationCommand::Amp(v),
            None => SimulationCommand::Unknown(trimmed.to_owned()),
        },
        "freq" => match parts.next().and_then(|v| v.parse::<f32>().ok()) {
            Some(v) => SimulationCommand::Freq(v),
            None => SimulationCommand::Unknown(trimmed.to_owned()),
        },
        "mode" => {
            let requested = match parts.next() {
                Some("live") => Some(Mode::LIVE),
                Some("idle") | Some("stop") | Some("stopped") => Some(Mode::IDLE),
                Some("record") | Some("recording") => Some(Mode::RECORDING),
                Some("play") | Some("playback") => Some(Mode::PLAYBACK),
                _ => None,
            };
            match requested {
                Some(mode) => SimulationCommand::Mode(mode),
                None => SimulationCommand::Unknown(trimmed.to_owned()),
            }
        }
        _ => SimulationCommand::Unknown(trimmed.to_owned()),
    }
}

fn parse_sampling_mode(raw: &str) -> Option<SamplingMode> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized == "captured" {
        return Some(SamplingMode::Captured);
    }
    if let Some(v) = normalized.strip_prefix("fixed:") {
        if let Ok(fps) = v.parse::<u32>() {
            if fps > 0 {
                return Some(SamplingMode::FixedFps { fps });
            }
        }
    }
    None
}

fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut locked = stdin.lock();
        loop {
            let mut line = String::new();
            match locked.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn print_embedded_banner(
    host: &str,
    port: u16,
    requested_bind: SocketAddr,
    route: &DemoRoute,
    client: &SimulatedClient,
) {
    sim_logln!("motionstage simulator ready");
    sim_logln!("mode: embedded server + simulated QUIC client");
    sim_logln!("requested bind: {requested_bind}");
    sim_logln!("server endpoint: {host}:{port}");
    sim_logln!("note: --server-bind configures embedded server bind address (not remote connect)");
    sim_logln!("source device: {}", route.source_device);
    sim_logln!("session id: {}", client.session_id);
    if let Some(target) = route.target.as_ref() {
        sim_logln!(
            "mapping: {} -> scene:{} object:{} attr:{}",
            route.source_output,
            target.scene_id,
            target.object_id,
            target.attribute_name
        );
    }
}

fn print_client_only_banner(
    connect_target: &ResolvedConnectTarget,
    route: &DemoRoute,
    client: &SimulatedClient,
) {
    sim_logln!("motionstage simulator ready");
    sim_logln!("mode: client-only (external server)");
    match connect_target {
        ResolvedConnectTarget::Direct(addr) => {
            sim_logln!("target server: {addr}");
        }
        ResolvedConnectTarget::Discovered {
            service_name,
            endpoint,
            host_name,
            protocol_major,
            protocol_minor,
        } => {
            sim_logln!(
                "target server: {endpoint} (discovered as `{service_name}`, host `{host_name}`)"
            );
            if let (Some(major), Some(minor)) = (protocol_major, protocol_minor) {
                sim_logln!("discovery protocol: {major}.{minor}");
            }
        }
    }
    sim_logln!("source device: {}", route.source_device);
    sim_logln!("session id: {}", client.session_id);
    sim_logln!("source output: {}", route.source_output);
    sim_logln!("note: ensure server-side scene/mapping/mode are configured");
}

fn print_help(mode: SimulationMode) {
    sim_logln!("commands:");
    sim_logln!("  start                 begin sine-wave streaming");
    sim_logln!("  stop                  stop streaming");
    sim_logln!("  record start [path]   start CMTRK recording (embedded mode only)");
    sim_logln!("  record stop           stop active recording (embedded mode only)");
    sim_logln!("  takes list            list available takes");
    sim_logln!("  takes select <id>     select active take");
    sim_logln!("  takes delete <id>     delete take");
    sim_logln!("  playback play         play selected take");
    sim_logln!("  playback pause        pause selected take");
    sim_logln!("  playback seek <ns>    seek selected take to nanoseconds");
    sim_logln!("  playback loop on|off  set looping behavior");
    sim_logln!("  playback stop         stop selected take playback");
    sim_logln!("  bake open <id> [captured|fixed:<fps>]  open bake cursor");
    sim_logln!("  bake next             read next bake frame");
    sim_logln!("  bake seek <index>     seek bake cursor frame index");
    sim_logln!("  bake close            close bake cursor");
    sim_logln!("  mapping create <object_id> <attribute> [scene_id]");
    sim_logln!(
        "                        create wire mapping from this device (default: active scene)"
    );
    sim_logln!("  mapping remove <id>   remove wire mapping (own-source or Operator)");
    sim_logln!("  take start            start wire take recording (server-assigned identity)");
    sim_logln!("  take stop             stop wire take and register it in the catalog");
    sim_logln!("  snapshot              request an on-demand scene snapshot");
    sim_logln!("  amp <value>           set sine amplitude");
    sim_logln!("  freq <value>          set sine frequency in Hz");
    sim_logln!("  mode <idle|live|recording|playback>  request runtime mode");
    sim_logln!("  status                print simulator state");
    sim_logln!("  help                  print this help");
    sim_logln!("  quit                  exit simulator");
    if matches!(mode, SimulationMode::ClientOnly) {
        sim_logln!("note: `record`, `takes`, `playback`, and `bake` commands are unavailable in client-only mode");
        sim_logln!("note: `mapping`, `take`, and `snapshot` are wire ops and work in both modes");
    }
}

fn print_prompt() {
    print!("motionstage-sim> ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

async fn print_status(
    server: Option<&ServerHandle>,
    route: &DemoRoute,
    state: &SimulatorState,
) -> Result<()> {
    sim_logln!("status:");
    sim_logln!("  streaming: {}", state.streaming);
    sim_logln!("  recording: {}", state.recording);
    sim_logln!("  sample_index: {}", state.sample_index);
    sim_logln!("  amplitude: {}", state.amplitude);
    sim_logln!("  frequency_hz: {}", state.frequency_hz);
    sim_logln!("  source_device: {}", route.source_device);
    sim_logln!("  source_output: {}", route.source_output);
    sim_logln!(
        "  selected_take: {}",
        state
            .selected_take
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
    sim_logln!("  playback_looping: {}", state.playback_looping);
    sim_logln!(
        "  active_bake_cursor: {}",
        state
            .active_bake_cursor
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
    sim_logln!(
        "  active_wire_take: {}",
        state
            .active_wire_take
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
    sim_logln!(
        "  last_error: {}",
        state.last_error.as_deref().unwrap_or("<none>")
    );

    let Some(server) = server else {
        sim_logln!("  mode: <unavailable in client-only mode>");
        sim_logln!("  metrics: <unavailable in client-only mode>");
        return Ok(());
    };

    let snapshot = server.last_published_snapshot().await;
    let metrics = server.metrics().await;
    let mode = snapshot.as_ref().and_then(|v| v.mode).unwrap_or(Mode::IDLE);

    sim_logln!("  mode: {mode:?}");
    sim_logln!(
        "  metrics: datagrams={}, updates={}, ticks={}, publishes={}",
        metrics.motion_datagrams,
        metrics.motion_updates,
        metrics.scheduler_ticks,
        metrics.publish_ticks
    );

    let value = route.target.as_ref().and_then(|target| {
        snapshot
            .as_ref()
            .and_then(|s| s.scenes.get(&target.scene_id))
            .and_then(|scene| scene.objects.get(&target.object_id))
            .and_then(|object| object.attributes.get(&target.attribute_name))
            .map(|attr| attr.current_value.clone())
    });

    if let Some(AttributeValue::Vec3f(v)) = value {
        sim_logln!("  mapped value: [{:.3}, {:.3}, {:.3}]", v[0], v[1], v[2]);
    } else {
        sim_logln!("  mapped value: <unavailable>");
    }
    Ok(())
}

async fn request_remote_mode(
    client: &mut SimulatedClient,
    mode: Mode,
    verbose: bool,
) -> Result<Mode> {
    // Order matters: stop recording/playback before reducing data flow,
    // but start data flow before enabling recording/playback.
    let messages: Vec<ControlMessage> = if mode.recording == RecordingState::Inactive {
        vec![
            ControlMessage::SetRecording(mode.recording),
            ControlMessage::SetDataFlow(mode.data_flow),
        ]
    } else {
        vec![
            ControlMessage::SetDataFlow(mode.data_flow),
            ControlMessage::SetRecording(mode.recording),
        ]
    };

    let mut last_mode = mode;
    for msg in messages {
        if verbose {
            sim_logln!("debug: sending {msg:?}");
        }
        client.control.send(&msg).await?;
        // Retired ack (protocol 2.1): success has no direct reply — the
        // acknowledgement is our own replicated ModeChanged echo; failure
        // arrives as ControlMessage::Error. Other sessions' events (and any
        // in-flight Pong/ModeState heartbeat reply) interleave on the control
        // stream; log and skip them while waiting for the echo.
        loop {
            match recv_control_with_timeout(client, "mode request").await? {
                ControlMessage::StateEventMsg(envelope) => {
                    let own_mode_echo = envelope.origin_session == Some(client.session_id)
                        && matches!(envelope.event, StateEvent::ModeChanged { .. });
                    log_state_event(&envelope);
                    if let (true, StateEvent::ModeChanged { mode }) =
                        (own_mode_echo, envelope.event)
                    {
                        last_mode = mode;
                        break;
                    }
                }
                ControlMessage::SceneSnapshot(snapshot) => log_scene_snapshot(&snapshot),
                ControlMessage::ModeState(active_mode) => {
                    if verbose {
                        sim_logln!("debug: received ControlMessage::ModeState({active_mode:?})");
                    }
                }
                ControlMessage::Pong => {
                    if verbose {
                        sim_logln!("debug: received ControlMessage::Pong");
                    }
                }
                ControlMessage::Error { code, reason } => {
                    return Err(anyhow!(
                        "remote mode request rejected: code={code:?} reason={reason}"
                    ));
                }
                other => {
                    return Err(anyhow!(
                        "unexpected control reply to mode request: {other:?}"
                    ));
                }
            }
        }
    }
    Ok(last_mode)
}

/// Bound on every reply wait so a lost message fails the command loudly
/// instead of hanging the interactive prompt.
const WIRE_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

async fn recv_control_with_timeout(
    client: &mut SimulatedClient,
    op: &str,
) -> Result<ControlMessage> {
    timeout(WIRE_REPLY_TIMEOUT, client.control.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for {op} reply"))?
        .map_err(|err| anyhow!("control channel receive failed: {err}"))
}

/// Wait for a direct operator-plane reply matched by `is_reply`, logging the
/// replicated state events that may interleave with it on the control stream
/// (direct replies and event echoes arrive in any order — match on message
/// type, not position).
async fn wait_for_wire_reply(
    client: &mut SimulatedClient,
    op: &str,
    verbose: bool,
    is_reply: impl Fn(&ControlMessage) -> bool,
) -> Result<ControlMessage> {
    loop {
        let message = recv_control_with_timeout(client, op).await?;
        if is_reply(&message) {
            return Ok(message);
        }
        match message {
            ControlMessage::StateEventMsg(envelope) => log_state_event(&envelope),
            ControlMessage::SceneSnapshot(snapshot) => log_scene_snapshot(&snapshot),
            ControlMessage::ModeState(active_mode) => {
                if verbose {
                    sim_logln!("debug: received ControlMessage::ModeState({active_mode:?})");
                }
            }
            ControlMessage::Pong => {
                if verbose {
                    sim_logln!("debug: received ControlMessage::Pong");
                }
            }
            ControlMessage::Error { code, reason } => {
                return Err(anyhow!("{op} rejected: code={code:?} reason={reason}"));
            }
            other => {
                return Err(anyhow!("unexpected control reply to {op}: {other:?}"));
            }
        }
    }
}

/// Render a replicated state event as a one-line interactive log entry.
fn describe_state_event(event: &StateEvent) -> String {
    match event {
        StateEvent::ModeChanged { mode } => format!("mode changed to {mode:?}"),
        StateEvent::SceneLoaded { scene_id, name } => {
            format!("scene loaded \"{name}\" ({scene_id})")
        }
        StateEvent::SceneActivated { scene_id } => format!("scene activated {scene_id}"),
        StateEvent::MappingCreated { mapping } => format!(
            "mapping created {} ({} -> {}.{})",
            mapping.mapping_id,
            mapping.source_output,
            mapping.target_object,
            mapping.target_attribute
        ),
        StateEvent::MappingUpdated { mapping } => format!(
            "mapping updated {} ({} -> {}.{})",
            mapping.mapping_id,
            mapping.source_output,
            mapping.target_object,
            mapping.target_attribute
        ),
        StateEvent::MappingRemoved { mapping_id } => format!("mapping removed {mapping_id}"),
        StateEvent::MappingLockChanged { mapping_id, lock } => {
            format!("mapping {mapping_id} lock={lock}")
        }
        StateEvent::MappingReleased { mapping_id, reason } => {
            format!("mapping released {mapping_id} ({reason})")
        }
        StateEvent::BaselineApplied {
            action,
            changed_attributes,
        } => format!("baseline {action:?} applied (changed_attributes={changed_attributes})"),
        StateEvent::SessionJoined {
            session_id,
            device_name,
            roles,
            ..
        } => format!("session joined \"{device_name}\" ({session_id}, roles={roles:?})"),
        StateEvent::SessionLeft { session_id, reason } => format!(
            "session left {session_id} (reason={})",
            reason.as_deref().unwrap_or("none")
        ),
        StateEvent::RecordingStarted { take_id, scene_id } => {
            format!("recording started take={take_id} scene={scene_id}")
        }
        StateEvent::RecordingStopped {
            take_id,
            frame_count,
        } => format!("recording stopped take={take_id} frames={frame_count}"),
        StateEvent::TakeRegistered { take } => {
            format!("take registered {} \"{}\"", take.take_id, take.name)
        }
        StateEvent::TakeSelected { take_id, .. } => format!("take selected {take_id}"),
        StateEvent::TakeDeleted { take_id } => format!("take deleted {take_id}"),
        StateEvent::TakeUpdated { take } => {
            format!(
                "take updated {} \"{}\" rating={:?}",
                take.take_id, take.name, take.rating
            )
        }
        StateEvent::PlaybackChanged {
            state,
            take_id,
            playhead_ns,
            looping,
            blocked_attributes,
        } => {
            format!(
                "playback {state:?} take={take_id} playhead_ns={playhead_ns} looping={looping} blocked={blocked_attributes}"
            )
        }
    }
}

fn log_state_event(envelope: &StateEventEnvelope) {
    let origin = envelope
        .origin_session
        .map(|id| id.to_string())
        .unwrap_or_else(|| "server".into());
    sim_logln!(
        "state event #{} [origin {}] {}",
        envelope.seq,
        origin,
        describe_state_event(&envelope.event)
    );
}

fn log_scene_snapshot(snapshot: &SceneSnapshotPayload) {
    sim_logln!(
        "scene snapshot seq={} scenes={} mappings={} sessions={} takes={} playback={} mode={:?} active_scene={}",
        snapshot.seq,
        snapshot.scenes.len(),
        snapshot.mappings.len(),
        snapshot.sessions.len(),
        snapshot.takes.len(),
        snapshot
            .playback
            .map(|playback| format!("{:?}", playback.state))
            .unwrap_or_else(|| "<none>".into()),
        snapshot.mode,
        snapshot
            .active_scene
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
}

/// Expand a snapshot into pickable targets: scene/object ids plus attribute
/// names (for `mapping create`), current mappings, and the take catalog.
fn print_snapshot_details(snapshot: &SceneSnapshotPayload) {
    for scene in &snapshot.scenes {
        let active = if snapshot.active_scene == Some(scene.scene_id) {
            " (active)"
        } else {
            ""
        };
        sim_logln!("  scene {} \"{}\"{}", scene.scene_id, scene.name, active);
        for object in &scene.objects {
            let attributes = object
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            sim_logln!(
                "    object {} \"{}\" attrs: [{}]",
                object.object_id,
                object.name,
                attributes
            );
        }
    }
    for mapping in &snapshot.mappings {
        sim_logln!(
            "  mapping {} {} -> {}.{} lock={}",
            mapping.mapping_id,
            mapping.source_output,
            mapping.target_object,
            mapping.target_attribute,
            mapping.lock
        );
    }
    for take in &snapshot.takes {
        sim_logln!(
            "  take {} \"{}\" frames={} selected={}",
            take.take_id,
            take.name,
            take.frame_count,
            take.selected
        );
    }
}

fn sine_vec3(amplitude: f32, frequency_hz: f32, seconds: f32) -> [f32; 3] {
    let phase = seconds * frequency_hz * 2.0 * PI;
    [
        amplitude * phase.sin(),
        amplitude * (phase + FRAC_PI_2).sin(),
        amplitude * (phase + PI).sin(),
    ]
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_nanos() as u64)
        .unwrap_or_default()
}

async fn connect_simulated_client(
    server_addr: SocketAddr,
    source_device: Uuid,
    output_attribute: &str,
    verbose: bool,
) -> Result<SimulatedClient> {
    if verbose {
        sim_logln!("debug: connecting QUIC peer to {server_addr}");
        sim_logln!(
            "debug: advertising MotionSource device={} output_attribute={}",
            source_device,
            output_attribute
        );
    }
    let client = QuicClient::new_insecure_for_local_dev()?;
    let peer = client.connect(server_addr).await?;
    if verbose {
        sim_logln!("debug: QUIC connection established");
    }
    let mut control = peer.accept_control_stream().await?;
    if verbose {
        sim_logln!("debug: control stream accepted");
    }

    match control.recv().await? {
        ControlMessage::ServerHello(hello) => {
            if verbose {
                sim_logln!(
                    "debug: received ServerHello proto={}.{} features={:?}",
                    hello.protocol_major,
                    hello.protocol_minor,
                    hello.features
                );
            }
        }
        other => {
            return Err(anyhow!(
                "expected ServerHello as first control message, got {other:?}"
            ));
        }
    }

    control
        .send(&ControlMessage::ClientHello(ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            device_id: source_device,
            device_name: "simulated-motion-device".into(),
            roles: vec![ClientRole::MotionSource, ClientRole::Operator],
            features: vec![Feature::Motion, Feature::Mapping, Feature::Recording],
            advertised_attributes: vec![AttributeDescriptor {
                path: output_attribute.to_owned(),
                value_type: AttributeKind::Vec3f,
            }],
        }))
        .await?;
    if verbose {
        sim_logln!("debug: sent ClientHello");
    }

    control
        .send(&ControlMessage::RegisterRequest(RegisterRequest {
            pairing_token: None,
            api_key: None,
        }))
        .await?;
    if verbose {
        sim_logln!("debug: sent RegisterRequest");
    }

    let session_id = match control.recv().await? {
        ControlMessage::RegisterAccepted(accepted) => {
            if verbose {
                sim_logln!(
                    "debug: registration accepted, negotiated_features={:?}",
                    accepted.negotiated_features
                );
                sim_logln!("debug: session_id={}", accepted.session_id);
            }
            accepted.session_id
        }
        ControlMessage::RegisterRejected(rejected) => {
            return Err(anyhow!(
                "simulated client registration rejected: code={:?} reason={}",
                rejected.code,
                rejected.reason
            ));
        }
        other => {
            return Err(anyhow!("expected registration result, got {other:?}"));
        }
    };

    // The server sends the initial world snapshot directly after
    // registration, before activating the session.
    match control.recv().await? {
        ControlMessage::SceneSnapshot(snapshot) => {
            log_scene_snapshot(&snapshot);
        }
        other => {
            return Err(anyhow!(
                "expected initial SceneSnapshot after registration, got {other:?}"
            ));
        }
    }

    Ok(SimulatedClient {
        _endpoint: client,
        peer,
        control,
        session_id,
    })
}

async fn drain_control_messages(
    client: &mut SimulatedClient,
    state: &mut SimulatorState,
    verbose: bool,
) -> Result<()> {
    loop {
        let recv = timeout(Duration::from_millis(1), client.control.recv()).await;
        let message = match recv {
            Ok(Ok(message)) => message,
            Ok(Err(err)) => return Err(anyhow!("recv failed: {err}")),
            Err(_) => break,
        };

        match message {
            ControlMessage::StateEventMsg(envelope) => {
                log_state_event(&envelope);
            }
            ControlMessage::SceneSnapshot(snapshot) => {
                log_scene_snapshot(&snapshot);
            }
            ControlMessage::ModeState(active_mode) => {
                if verbose {
                    sim_logln!("debug: async control message ModeState({active_mode:?})");
                }
            }
            ControlMessage::Error { code, reason } => {
                let rendered = format!("control error code={code:?} reason={reason}");
                sim_logln!("{rendered}");
                state.last_error = Some(rendered);
            }
            ControlMessage::Pong => {
                if verbose {
                    sim_logln!("debug: async control message Pong");
                }
            }
            other => {
                if verbose {
                    sim_logln!("debug: async control message {other:?}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use motionstage_protocol::SessionState;
    use std::net::IpAddr;

    #[test]
    fn parse_record_start_with_path() {
        assert_eq!(
            parse_command("record start /tmp/demo.cmtrk"),
            SimulationCommand::RecordStart(Some(PathBuf::from("/tmp/demo.cmtrk")))
        );
    }

    #[test]
    fn parse_tunable_commands() {
        assert_eq!(parse_command("amp 2.5"), SimulationCommand::Amp(2.5));
        assert_eq!(parse_command("freq 1.25"), SimulationCommand::Freq(1.25));
    }

    #[test]
    fn parse_take_and_playback_commands() {
        let take_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
        assert_eq!(parse_command("takes list"), SimulationCommand::TakesList);
        assert_eq!(
            parse_command("takes select 00000000-0000-0000-0000-000000000123"),
            SimulationCommand::TakesSelect(take_id)
        );
        assert_eq!(
            parse_command("playback play"),
            SimulationCommand::PlaybackPlay
        );
        assert_eq!(
            parse_command("playback pause"),
            SimulationCommand::PlaybackPause
        );
        assert_eq!(
            parse_command("playback seek 1000"),
            SimulationCommand::PlaybackSeek(1000)
        );
        assert_eq!(
            parse_command("playback loop on"),
            SimulationCommand::PlaybackLoop(true)
        );
    }

    #[test]
    fn parse_bake_commands() {
        let take_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
        assert_eq!(
            parse_command("bake open 00000000-0000-0000-0000-000000000123"),
            SimulationCommand::BakeOpen(take_id, SamplingMode::Captured)
        );
        assert_eq!(
            parse_command("bake open 00000000-0000-0000-0000-000000000123 fixed:24"),
            SimulationCommand::BakeOpen(take_id, SamplingMode::FixedFps { fps: 24 })
        );
        assert_eq!(parse_command("bake next"), SimulationCommand::BakeNext);
        assert_eq!(parse_command("bake seek 5"), SimulationCommand::BakeSeek(5));
        assert_eq!(parse_command("bake close"), SimulationCommand::BakeClose);
    }

    #[test]
    fn parse_wire_operator_commands() {
        let object_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
        let scene_id = Uuid::parse_str("00000000-0000-0000-0000-000000000456").unwrap();
        assert_eq!(
            parse_command("mapping create 00000000-0000-0000-0000-000000000123 position"),
            SimulationCommand::MappingCreate {
                object_id,
                attribute: "position".to_owned(),
                scene_id: None,
            }
        );
        assert_eq!(
            parse_command(
                "mapping create 00000000-0000-0000-0000-000000000123 position \
                 00000000-0000-0000-0000-000000000456"
            ),
            SimulationCommand::MappingCreate {
                object_id,
                attribute: "position".to_owned(),
                scene_id: Some(scene_id),
            }
        );
        assert_eq!(
            parse_command("mapping remove 00000000-0000-0000-0000-000000000123"),
            SimulationCommand::MappingRemove(object_id)
        );
        assert_eq!(parse_command("take start"), SimulationCommand::TakeStart);
        assert_eq!(parse_command("take stop"), SimulationCommand::TakeStop);
        assert_eq!(parse_command("snapshot"), SimulationCommand::Snapshot);
        assert!(matches!(
            parse_command("mapping create not-a-uuid position"),
            SimulationCommand::Unknown(_)
        ));
        assert!(matches!(
            parse_command("take"),
            SimulationCommand::Unknown(_)
        ));
    }

    #[test]
    fn sine_vec3_uses_expected_phase_offsets() {
        let v = sine_vec3(2.0, 0.0, 10.0);
        assert!((v[0] - 0.0).abs() < 0.0001);
        assert!((v[1] - 2.0).abs() < 0.0001);
        assert!((v[2] - 0.0).abs() < 0.0001);
    }

    #[test]
    fn parse_connect_target_accepts_socket_addr() {
        let parsed = parse_connect_target("127.0.0.1:7788").unwrap();
        match parsed {
            ConnectTarget::Address(addr) => {
                assert_eq!(addr, SocketAddr::from_str("127.0.0.1:7788").unwrap());
            }
            ConnectTarget::Discover(_) => panic!("expected address connect target"),
        }
    }

    #[test]
    fn parse_connect_target_accepts_discover_keyword() {
        let parsed = parse_connect_target("discover").unwrap();
        match parsed {
            ConnectTarget::Discover(name) => assert_eq!(name, None),
            ConnectTarget::Address(_) => panic!("expected discover connect target"),
        }
    }

    #[test]
    fn parse_connect_target_accepts_named_discover_target() {
        let parsed = parse_connect_target("discover:motionstage-blender").unwrap();
        match parsed {
            ConnectTarget::Discover(name) => {
                assert_eq!(name.as_deref(), Some("motionstage-blender"))
            }
            ConnectTarget::Address(_) => panic!("expected discover connect target"),
        }
    }

    #[test]
    fn qualify_source_output_prefixes_device_id_when_needed() {
        let device_id = Uuid::parse_str("00000000-0000-0000-0000-000000000111").unwrap();
        assert_eq!(
            qualify_source_output(device_id, "demo.position"),
            "00000000-0000-0000-0000-000000000111.demo.position"
        );
        assert_eq!(
            qualify_source_output(
                device_id,
                "00000000-0000-0000-0000-000000000111.demo.position"
            ),
            "00000000-0000-0000-0000-000000000111.demo.position"
        );
        assert_eq!(
            qualify_source_output(
                device_id,
                "00000000-0000-0000-0000-000000000999.demo.position"
            ),
            "00000000-0000-0000-0000-000000000999.demo.position"
        );
    }

    #[test]
    fn select_preferred_endpoint_prefers_ipv4_non_loopback() {
        let service = DiscoveredService {
            service_name: "demo".to_owned(),
            fullname: "demo._motionstage._udp.local.".to_owned(),
            host_name: "demo.local.".to_owned(),
            addresses: vec![
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "::1".parse::<IpAddr>().unwrap(),
                "192.168.1.20".parse::<IpAddr>().unwrap(),
            ],
            port: 7788,
            protocol_major: Some(1),
            protocol_minor: Some(2),
            cert_fingerprint: None,
        };

        let endpoint = select_preferred_endpoint(&service).unwrap();
        assert_eq!(endpoint, SocketAddr::from_str("192.168.1.20:7788").unwrap());
    }

    #[tokio::test]
    async fn simulated_client_connects_via_quic_handshake() {
        let config = ServerConfig {
            quic_bind_addr: "127.0.0.1:0".parse().unwrap(),
            enable_discovery: false,
            ..Default::default()
        };
        let server = ServerHandle::new(config);
        let _adv = server.start().await.unwrap();
        let server_addr = server.quic_bind_addr().await;

        let source_device = Uuid::now_v7();
        let client = connect_simulated_client(server_addr, source_device, "demo.position", false)
            .await
            .unwrap();

        let session = server.session_info(source_device).await.unwrap();
        assert_eq!(session.state, SessionState::Active);

        drop(client);
        server.stop().await.unwrap();
    }
}
