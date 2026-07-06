use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use motionstage_export_usd::UsdExportOptions;

#[derive(Debug, Clone, Args)]
pub struct ExportArgs {
    /// Input CMTRK recording file (`.cmtrk`).
    pub input: PathBuf,
    /// Output format.
    #[arg(long, value_enum)]
    pub format: ExportFormat,
    /// Output file path; prints to stdout when omitted.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// USD timeCodesPerSecond (ignored for `chan`).
    #[arg(long, default_value_t = 120)]
    pub fps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    Usd,
    Chan,
}

pub fn run(args: &ExportArgs) -> Result<()> {
    let text = render(args)?;
    match args.output.as_deref() {
        Some(path) => write_output(path, &text)
            .with_context(|| format!("failed to write export to `{}`", path.display()))?,
        None => {
            let mut stdout = std::io::stdout().lock();
            let result = stdout.write_all(text.as_bytes()).and_then(|()| {
                if text.ends_with('\n') {
                    Ok(())
                } else {
                    stdout.write_all(b"\n")
                }
            });
            match result {
                // A closed pipe (e.g. `... | head`) is not an export failure.
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
                other => other?,
            }
        }
    }
    Ok(())
}

/// Read the recording and render it in the requested format.
pub fn render(args: &ExportArgs) -> Result<String> {
    // fps only feeds USD's timeCodesPerSecond; `chan` ignores it, as the
    // help text says, so don't reject it there.
    if args.fps == 0 && args.format == ExportFormat::Usd {
        return Err(anyhow!("--fps must be greater than zero for --format usd"));
    }
    let recording = motionstage_recording::read_recording(&args.input)
        .with_context(|| format!("failed to read recording `{}`", args.input.display()))?;
    Ok(match args.format {
        ExportFormat::Usd => motionstage_export_usd::export_with_options(
            &recording,
            &UsdExportOptions {
                time_codes_per_second: args.fps,
                ..UsdExportOptions::default()
            },
        ),
        ExportFormat::Chan => motionstage_export_chan::export(&recording),
    })
}

fn write_output(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use motionstage_core::AttributeValue;
    use motionstage_protocol::Mode;
    use motionstage_recording::{RecordedAttribute, RecordedFrame, RecordingWriter};
    use uuid::Uuid;

    fn write_fixture_recording(path: &Path) {
        let object_id = Uuid::nil();
        let mut writer = RecordingWriter::start(Uuid::nil(), 0);
        for i in 0..3u64 {
            writer.push_frame(RecordedFrame {
                timestamp_ns: i * 25_000_000,
                mode: Mode::RECORDING,
                attributes: vec![RecordedAttribute {
                    object_id,
                    attribute: "position".into(),
                    value: AttributeValue::Vec3f([i as f32, 0.0, 1.5]),
                }],
            });
        }
        writer.finish(path).unwrap();
    }

    #[test]
    fn export_command_writes_usd_to_output_path() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("take.cmtrk");
        let output = dir.path().join("out/take.usda");
        write_fixture_recording(&input);

        run(&ExportArgs {
            input,
            format: ExportFormat::Usd,
            output: Some(output.clone()),
            fps: 120,
        })
        .unwrap();

        let usda = fs::read_to_string(&output).unwrap();
        assert!(usda.starts_with("#usda 1.0\n"));
        assert!(usda.contains("timeCodesPerSecond = 120"));
        assert!(usda.contains("double3 xformOp:translate.timeSamples = {"));
        assert!(usda.contains("3: (1, 0, 1.5),"));
    }

    #[test]
    fn export_command_renders_chan_format() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("take.cmtrk");
        write_fixture_recording(&input);

        let chan = render(&ExportArgs {
            input,
            format: ExportFormat::Chan,
            output: None,
            fps: 120,
        })
        .unwrap();
        assert!(chan.contains("position.tx"));
    }

    #[test]
    fn export_command_honors_fps() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("take.cmtrk");
        write_fixture_recording(&input);

        let usda = render(&ExportArgs {
            input,
            format: ExportFormat::Usd,
            output: None,
            fps: 24,
        })
        .unwrap();
        assert!(usda.contains("timeCodesPerSecond = 24"));
    }

    #[test]
    fn export_command_rejects_zero_fps_for_usd() {
        let err = render(&ExportArgs {
            input: PathBuf::from("does-not-matter.cmtrk"),
            format: ExportFormat::Usd,
            output: None,
            fps: 0,
        })
        .unwrap_err();
        assert!(err.to_string().contains("--fps"));
    }

    #[test]
    fn export_command_ignores_zero_fps_for_chan() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("take.cmtrk");
        write_fixture_recording(&input);

        // The help text says fps is ignored for `chan`, so fps=0 must not be
        // rejected there.
        let chan = render(&ExportArgs {
            input,
            format: ExportFormat::Chan,
            output: None,
            fps: 0,
        })
        .unwrap();
        assert!(chan.contains("position.tx"));
    }
}
