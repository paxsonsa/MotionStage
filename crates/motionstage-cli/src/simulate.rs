use std::{
    f32::consts::{FRAC_PI_2, PI},
    fs,
    io::BufRead,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use clap::Args;
use motionstage_core::{
    AttributeUpdate, AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject,
};
use motionstage_protocol::{
    ClientHello, ClientRole, Feature, Mode, RegisterRequest, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use motionstage_server::{ServerConfig, ServerHandle};
use tokio::{
    sync::mpsc,
    time::{interval, MissedTickBehavior},
};
use uuid::Uuid;

#[derive(Debug, Clone, Args)]
pub struct SimulateArgs {
    #[arg(long, default_value = "motionstage-sim")]
    pub name: String,
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: SocketAddr,
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
}

#[derive(Debug, Clone)]
struct DemoMapping {
    source_device: Uuid,
    source_output: String,
    scene_id: Uuid,
    object_id: Uuid,
    attribute_name: String,
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
    Amp(f32),
    Freq(f32),
    Quit,
    Empty,
    Unknown(String),
}

pub async fn run(args: SimulateArgs) -> Result<()> {
    validate_args(&args)?;

    let mut config = ServerConfig::default();
    config.name = args.name.clone();
    config.quic_bind_addr = args.bind;
    config.enable_discovery = args.discoverable;

    let server = ServerHandle::new(config);
    let advertisement = server.start().await?;

    let mapping = bootstrap_demo_mapping(&server).await?;
    let mut state = SimulatorState::new(&args);

    print_banner(&advertisement.bind_host, advertisement.bind_port, &mapping);
    print_help();
    print_prompt();

    let mut lines = spawn_stdin_reader();
    let tick_ns = (1_000_000_000_u64 / args.sample_hz as u64).max(1);
    let mut sample_interval = interval(Duration::from_nanos(tick_ns));
    sample_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = sample_interval.tick() => {
                if state.streaming {
                    emit_sample(
                        &server,
                        &mapping,
                        &mut state,
                        args.sample_hz,
                    ).await?;
                }
            }
            line = lines.recv() => {
                let Some(line) = line else {
                    break;
                };
                let command = parse_command(&line);
                let should_continue = handle_command(&server, &mapping, &mut state, command).await?;
                if !should_continue {
                    break;
                }
                print_prompt();
            }
            _ = &mut ctrl_c => {
                println!("\nreceived ctrl+c, shutting down simulator");
                break;
            }
        }
    }

    if state.recording {
        let manifest = server.stop_recording().await?;
        println!(
            "recording stopped (id={}, frames={})",
            manifest.recording_id, manifest.frame_count
        );
    }
    if state.streaming {
        server.set_mode(Mode::Idle).await?;
    }
    server.stop().await?;
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
    Ok(())
}

async fn bootstrap_demo_mapping(server: &ServerHandle) -> Result<DemoMapping> {
    let source_device = Uuid::now_v7();
    let source_output = "demo.position".to_owned();
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

    server
        .discovered(source_device, "simulated-motion-device")
        .await?;
    server.transport_connected(source_device).await?;
    server
        .hello_exchanged(ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            device_id: source_device,
            device_name: "simulated-motion-device".into(),
            roles: vec![ClientRole::MotionSource, ClientRole::Operator],
            features: vec![Feature::Motion, Feature::Mapping, Feature::Recording],
        })
        .await?;
    server.authenticate(source_device).await?;
    let _accepted = server
        .register(
            source_device,
            RegisterRequest {
                pairing_token: None,
                api_key: None,
            },
        )
        .await?;
    server.scene_synced(source_device).await?;
    server.activate(source_device).await?;

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

    Ok(DemoMapping {
        source_device,
        source_output,
        scene_id,
        object_id,
        attribute_name,
    })
}

async fn emit_sample(
    server: &ServerHandle,
    mapping: &DemoMapping,
    state: &mut SimulatorState,
    sample_hz: u32,
) -> Result<()> {
    let t = state.sample_index as f32 / sample_hz as f32;
    let value = sine_vec3(state.amplitude, state.frequency_hz, t);

    server
        .ingest_motion_samples(
            mapping.source_device,
            vec![AttributeUpdate {
                output_attribute: mapping.source_output.clone(),
                value: AttributeValue::Vec3f(value),
            }],
            now_ns(),
        )
        .await?;

    if state.sample_index % state.print_every as u64 == 0 {
        println!(
            "sample {:>8} -> [{:>7.3}, {:>7.3}, {:>7.3}]",
            state.sample_index, value[0], value[1], value[2]
        );
    }

    state.sample_index += 1;
    Ok(())
}

async fn handle_command(
    server: &ServerHandle,
    mapping: &DemoMapping,
    state: &mut SimulatorState,
    command: SimulationCommand,
) -> Result<bool> {
    match command {
        SimulationCommand::Help => {
            print_help();
        }
        SimulationCommand::Start => {
            server.set_mode(Mode::Live).await?;
            state.streaming = true;
            println!("streaming started (mode=Live)");
        }
        SimulationCommand::Stop => {
            if state.recording {
                println!("recording is active, run `record stop` first");
                return Ok(true);
            }
            state.streaming = false;
            server.set_mode(Mode::Idle).await?;
            println!("streaming stopped (mode=Idle)");
        }
        SimulationCommand::Status => {
            print_status(server, mapping, state).await?;
        }
        SimulationCommand::RecordStart(path) => {
            if state.recording {
                println!("recording is already active");
                return Ok(true);
            }
            if !state.streaming {
                server.set_mode(Mode::Live).await?;
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
            println!(
                "recording started (id={recording_id}, path={})",
                path.display()
            );
        }
        SimulationCommand::RecordStop => {
            if !state.recording {
                println!("no active recording");
                return Ok(true);
            }
            let manifest = server.stop_recording().await?;
            state.recording = false;
            let path = state
                .active_record_path
                .as_ref()
                .map(|v| v.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());
            println!(
                "recording stopped (id={}, frames={}, path={})",
                manifest.recording_id, manifest.frame_count, path
            );
            state.active_record_path = None;
        }
        SimulationCommand::Amp(value) => {
            if value < 0.0 || !value.is_finite() {
                println!("amplitude must be a finite number >= 0");
            } else {
                state.amplitude = value;
                println!("amplitude set to {}", state.amplitude);
            }
        }
        SimulationCommand::Freq(value) => {
            if value < 0.0 || !value.is_finite() {
                println!("frequency must be a finite number >= 0");
            } else {
                state.frequency_hz = value;
                println!("frequency set to {}", state.frequency_hz);
            }
        }
        SimulationCommand::Quit => {
            println!("exiting simulator");
            return Ok(false);
        }
        SimulationCommand::Empty => {}
        SimulationCommand::Unknown(value) => {
            println!("unknown command `{value}` (run `help`)");
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
        "amp" => match parts.next().and_then(|v| v.parse::<f32>().ok()) {
            Some(v) => SimulationCommand::Amp(v),
            None => SimulationCommand::Unknown(trimmed.to_owned()),
        },
        "freq" => match parts.next().and_then(|v| v.parse::<f32>().ok()) {
            Some(v) => SimulationCommand::Freq(v),
            None => SimulationCommand::Unknown(trimmed.to_owned()),
        },
        _ => SimulationCommand::Unknown(trimmed.to_owned()),
    }
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

fn print_banner(host: &str, port: u16, mapping: &DemoMapping) {
    println!("motionstage simulator ready");
    println!("server endpoint: {host}:{port}");
    println!("source device: {}", mapping.source_device);
    println!(
        "mapping: {} -> scene:{} object:{} attr:{}",
        mapping.source_output, mapping.scene_id, mapping.object_id, mapping.attribute_name
    );
}

fn print_help() {
    println!("commands:");
    println!("  start                 set Live mode and begin sine-wave streaming");
    println!("  stop                  stop streaming and set Idle mode");
    println!("  record start [path]   start CMTRK recording");
    println!("  record stop           stop active recording");
    println!("  amp <value>           set sine amplitude");
    println!("  freq <value>          set sine frequency in Hz");
    println!("  status                print mode, metrics, and mapped vec3 value");
    println!("  help                  print this help");
    println!("  quit                  exit simulator");
}

fn print_prompt() {
    print!("motionstage-sim> ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

async fn print_status(
    server: &ServerHandle,
    mapping: &DemoMapping,
    state: &SimulatorState,
) -> Result<()> {
    let snapshot = server.last_published_snapshot().await;
    let metrics = server.metrics().await;

    let mode = snapshot.as_ref().and_then(|v| v.mode).unwrap_or(Mode::Idle);

    let value = snapshot
        .as_ref()
        .and_then(|s| s.scenes.get(&mapping.scene_id))
        .and_then(|scene| scene.objects.get(&mapping.object_id))
        .and_then(|object| object.attributes.get(&mapping.attribute_name))
        .map(|attr| attr.current_value.clone());

    println!("status:");
    println!("  mode: {mode:?}");
    println!("  streaming: {}", state.streaming);
    println!("  recording: {}", state.recording);
    println!("  sample_index: {}", state.sample_index);
    println!("  amplitude: {}", state.amplitude);
    println!("  frequency_hz: {}", state.frequency_hz);
    println!(
        "  metrics: datagrams={}, updates={}, ticks={}, publishes={}",
        metrics.motion_datagrams,
        metrics.motion_updates,
        metrics.scheduler_ticks,
        metrics.publish_ticks
    );
    if let Some(AttributeValue::Vec3f(v)) = value {
        println!("  mapped value: [{:.3}, {:.3}, {:.3}]", v[0], v[1], v[2]);
    } else {
        println!("  mapped value: <unavailable>");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sine_vec3_uses_expected_phase_offsets() {
        let v = sine_vec3(2.0, 0.0, 10.0);
        assert!((v[0] - 0.0).abs() < 0.0001);
        assert!((v[1] - 2.0).abs() < 0.0001);
        assert!((v[2] - 0.0).abs() < 0.0001);
    }
}
