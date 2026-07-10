use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use chrono::Local;
use clap::{Parser, ValueEnum};
use directories::UserDirs;
use serde::Deserialize;

mod telemetry;

use telemetry::{TelemetryKind, TelemetrySession};

const DEFAULT_DIRECTORY_NAME: &str = "hyprrec";
const VIDEO_EXTENSION: &str = ".mp4";

#[derive(Debug, Parser)]
#[command(
    name = "hyprrec",
    version,
    about = "High-quality screen recording for Hyprland",
    after_help = "Press Ctrl+C to stop recording and finalize the video."
)]
struct Cli {
    /// Interactively select a region with slurp
    #[arg(long)]
    region: bool,

    /// Record the default PipeWire audio source
    #[arg(long)]
    audio: bool,

    /// Directory in which to save recordings [default: $XDG_VIDEOS_DIR/hyprrec]
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,

    /// Output basename; .mp4 is added automatically
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Encoding profile: high is compatible, ultra maximizes detail, compact uses efficient HEVC
    #[arg(long, value_enum, default_value = "high")]
    quality: Quality,

    /// Write selected action telemetry beside the recording as JSONL
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        value_name = "CATEGORY",
        help = "Track action telemetry (repeat or comma-separate): click, scroll, keyboard-input, workspace-changed, window-focused"
    )]
    telemetry: Vec<TelemetryKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Quality {
    High,
    Ultra,
    Compact,
}

#[derive(Debug)]
enum AppError {
    Message(String),
    Cancelled,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::Cancelled => f.write_str("region selection cancelled"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        Self::Message(format!("could not parse Hyprland monitor data: {error}"))
    }
}

#[derive(Debug, Deserialize)]
struct Monitor {
    name: String,
    focused: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug)]
enum CaptureTarget {
    Output(String),
    Geometry(String),
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppError::Cancelled) => {
            eprintln!("hyprrec: region selection cancelled");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hyprrec: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), AppError> {
    validate_session()?;
    ensure_command_available("wf-recorder")?;
    ensure_command_available("hyprctl")?;
    if cli.region {
        ensure_command_available("slurp")?;
    }

    let directory = resolve_output_directory(cli.dir)?;
    fs::create_dir_all(&directory).map_err(|error| {
        AppError::Message(format!(
            "could not create output directory {}: {error}",
            directory.display()
        ))
    })?;

    let destination = build_output_path(&directory, cli.name.as_deref(), Local::now())?;
    ensure_destination_available(&destination)?;
    if !cli.telemetry.is_empty() {
        ensure_destination_available(&telemetry_path(&destination))?;
    }

    let target = if cli.region {
        CaptureTarget::Geometry(select_region()?)
    } else {
        CaptureTarget::Output(focused_output()?)
    };

    let args = recorder_args(&destination, &target, cli.audio, cli.quality);
    println!("Recording to {}", destination.display());
    let telemetry = if cli.telemetry.is_empty() {
        None
    } else {
        let session = TelemetrySession::start(&destination, &cli.telemetry)
            .map_err(|error| AppError::Message(format!("could not start telemetry: {error}")))?;
        println!("Telemetry will be saved to {}", session.path().display());
        Some(session)
    };
    println!("Press Ctrl+C to stop and finalize the recording.");

    // Both processes receive terminal Ctrl+C. Keeping the parent alive lets it
    // wait while wf-recorder handles the signal and finalizes the container.
    ctrlc::set_handler(|| {}).map_err(|error| {
        AppError::Message(format!("could not install the Ctrl+C handler: {error}"))
    })?;

    let status = Command::new("wf-recorder")
        .args(&args)
        .status()
        .map_err(|error| AppError::Message(format!("could not start wf-recorder: {error}")));

    if let Some(session) = &telemetry {
        session.stop(status.as_ref().is_ok_and(|status| status.success()));
    }

    let status = status?;

    if !status.success() {
        return Err(AppError::Message(format!(
            "wf-recorder exited with {status}; a partial file may remain at {}",
            destination.display()
        )));
    }

    println!("Saved recording to {}", destination.display());
    Ok(())
}

fn validate_session() -> Result<(), AppError> {
    if env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(AppError::Message(
            "WAYLAND_DISPLAY is not set; hyprrec must run inside a Wayland session".into(),
        ));
    }
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return Err(AppError::Message(
            "HYPRLAND_INSTANCE_SIGNATURE is not set; hyprrec must run inside Hyprland".into(),
        ));
    }
    Ok(())
}

fn ensure_command_available(command: &str) -> Result<(), AppError> {
    let path = env::var_os("PATH").ok_or_else(|| AppError::Message("PATH is not set".into()))?;

    let found = env::split_paths(&path).any(|directory| {
        let candidate = directory.join(command);
        fs::metadata(candidate)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    });

    if found {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "required command `{command}` was not found in PATH; run hyprrec through shell.nix"
        )))
    }
}

fn resolve_output_directory(override_directory: Option<PathBuf>) -> Result<PathBuf, AppError> {
    if let Some(directory) = override_directory {
        return Ok(directory);
    }

    let user_dirs = UserDirs::new().ok_or_else(|| {
        AppError::Message("could not determine the current user's home directory".into())
    })?;
    let videos = user_dirs
        .video_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| user_dirs.home_dir().join("Videos"));
    Ok(videos.join(DEFAULT_DIRECTORY_NAME))
}

fn build_output_path(
    directory: &Path,
    requested_name: Option<&str>,
    now: chrono::DateTime<Local>,
) -> Result<PathBuf, AppError> {
    let basename = match requested_name {
        Some(name) => validate_name(name)?,
        None => format!("hyprrec-{}", now.format("%Y%m%d-%H%M%S")),
    };

    let filename = if basename.to_ascii_lowercase().ends_with(VIDEO_EXTENSION) {
        basename
    } else {
        format!("{basename}{VIDEO_EXTENSION}")
    };
    Ok(directory.join(filename))
}

fn ensure_destination_available(destination: &Path) -> Result<(), AppError> {
    if destination.exists() {
        Err(AppError::Message(format!(
            "refusing to overwrite existing recording: {}",
            destination.display()
        )))
    } else {
        Ok(())
    }
}

fn telemetry_path(destination: &Path) -> PathBuf {
    destination.with_extension("telemetry.jsonl")
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(AppError::Message(
            "--name must contain a non-empty filename".into(),
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(AppError::Message(
            "--name must be a basename without path separators; use --dir for the folder".into(),
        ));
    }
    Ok(name.to_owned())
}

fn focused_output() -> Result<String, AppError> {
    let output = Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .map_err(|error| AppError::Message(format!("could not run hyprctl: {error}")))?;

    if !output.status.success() {
        return Err(AppError::Message(format!(
            "hyprctl could not list monitors (exit status {})",
            output.status
        )));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| AppError::Message(format!("hyprctl returned invalid UTF-8: {error}")))?;
    parse_focused_output(&stdout)
}

fn parse_focused_output(json: &str) -> Result<String, AppError> {
    let monitors: Vec<Monitor> = serde_json::from_str(json)?;
    monitors
        .into_iter()
        .find(|monitor| monitor.focused && !monitor.disabled)
        .map(|monitor| monitor.name)
        .ok_or_else(|| AppError::Message("Hyprland did not report a focused active monitor".into()))
}

fn select_region() -> Result<String, AppError> {
    let output = Command::new("slurp")
        .output()
        .map_err(|error| AppError::Message(format!("could not start slurp: {error}")))?;

    if !output.status.success() {
        return Err(AppError::Cancelled);
    }

    let geometry = String::from_utf8(output.stdout)
        .map_err(|error| AppError::Message(format!("slurp returned invalid UTF-8: {error}")))?;
    let geometry = geometry.trim();
    if geometry.is_empty() {
        return Err(AppError::Cancelled);
    }
    Ok(geometry.to_owned())
}

fn recorder_args(
    destination: &Path,
    target: &CaptureTarget,
    audio: bool,
    quality: Quality,
) -> Vec<OsString> {
    let (codec, pixel_format, crf, preset, extra_codec_param) = match quality {
        Quality::High => ("libx264", "yuv420p", "crf=18", "preset=veryfast", None),
        Quality::Ultra => ("libx264", "yuv444p", "crf=10", "preset=veryfast", None),
        Quality::Compact => (
            "libx265",
            "yuv444p",
            "crf=12",
            "preset=ultrafast",
            Some("x265-params=psy-rd=0:cbqpoffs=0:crqpoffs=0"),
        ),
    };

    let mut args = vec![
        OsString::from("-f"),
        destination.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from(codec),
        OsString::from("-r"),
        OsString::from("60"),
        OsString::from("-x"),
        OsString::from(pixel_format),
        OsString::from("-p"),
        OsString::from(crf),
        OsString::from("-p"),
        OsString::from(preset),
    ];

    if let Some(codec_param) = extra_codec_param {
        args.push(OsString::from("-p"));
        args.push(OsString::from(codec_param));
    }

    match target {
        CaptureTarget::Output(output) => {
            args.push(OsString::from("-o"));
            args.push(OsString::from(output));
        }
        CaptureTarget::Geometry(geometry) => {
            args.push(OsString::from("-g"));
            args.push(OsString::from(geometry));
        }
    }

    if audio {
        args.push(OsString::from("--audio-backend=pipewire"));
        args.push(OsString::from("--audio"));
    }
    args
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn args_as_strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn timestamped_output_path_uses_expected_format() {
        let now = Local.with_ymd_and_hms(2026, 7, 10, 9, 8, 7).unwrap();
        let path = build_output_path(Path::new("/videos"), None, now).unwrap();

        assert_eq!(path, Path::new("/videos/hyprrec-20260710-090807.mp4"));
    }

    #[test]
    fn custom_name_gets_mp4_extension() {
        let now = Local::now();
        let path = build_output_path(Path::new("recordings"), Some("demo"), now).unwrap();
        assert_eq!(path, Path::new("recordings/demo.mp4"));
    }

    #[test]
    fn existing_mp4_extension_is_not_duplicated() {
        let now = Local::now();
        let path = build_output_path(Path::new("recordings"), Some("DEMO.MP4"), now).unwrap();
        assert_eq!(path, Path::new("recordings/DEMO.MP4"));
    }

    #[test]
    fn custom_name_rejects_paths_and_empty_values() {
        for invalid in ["", "  ", ".", "..", "nested/demo", "nested\\demo"] {
            assert!(
                build_output_path(Path::new("recordings"), Some(invalid), Local::now()).is_err()
            );
        }
    }

    #[test]
    fn custom_output_directory_is_used_unchanged() {
        let directory = PathBuf::from("relative/recordings");
        assert_eq!(
            resolve_output_directory(Some(directory.clone())).unwrap(),
            directory
        );
    }

    #[test]
    fn existing_destination_is_rejected() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let destination = temporary_directory.path().join("existing.mp4");
        fs::write(&destination, b"existing recording").unwrap();

        let error = ensure_destination_available(&destination).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
    }

    #[test]
    fn focused_active_monitor_is_parsed() {
        let json = r#"[
            {"name":"DP-1","focused":false,"disabled":false},
            {"name":"eDP-1","focused":true,"disabled":false}
        ]"#;
        assert_eq!(parse_focused_output(json).unwrap(), "eDP-1");
    }

    #[test]
    fn disabled_or_missing_focus_is_rejected() {
        let json = r#"[{"name":"eDP-1","focused":true,"disabled":true}]"#;
        assert!(parse_focused_output(json).is_err());
        assert!(parse_focused_output("[]").is_err());
    }

    #[test]
    fn output_recording_uses_high_quality_profile() {
        let args = recorder_args(
            Path::new("/videos/demo.mp4"),
            &CaptureTarget::Output("eDP-1".into()),
            false,
            Quality::High,
        );

        assert_eq!(
            args_as_strings(&args),
            [
                "-f",
                "/videos/demo.mp4",
                "-c",
                "libx264",
                "-r",
                "60",
                "-x",
                "yuv420p",
                "-p",
                "crf=18",
                "-p",
                "preset=veryfast",
                "-o",
                "eDP-1",
            ]
        );
    }

    #[test]
    fn region_and_audio_arguments_are_added() {
        let args = recorder_args(
            Path::new("demo.mp4"),
            &CaptureTarget::Geometry("10,20 800x600".into()),
            true,
            Quality::High,
        );
        let args = args_as_strings(&args);

        assert!(args.windows(2).any(|pair| pair == ["-g", "10,20 800x600"]));
        assert!(args.contains(&"--audio-backend=pipewire"));
        assert!(args.contains(&"--audio"));
        assert!(!args.contains(&"-o"));
    }

    #[test]
    fn ultra_profile_preserves_full_chroma_detail() {
        let args = recorder_args(
            Path::new("demo.mp4"),
            &CaptureTarget::Output("eDP-1".into()),
            false,
            Quality::Ultra,
        );
        let args = args_as_strings(&args);

        assert!(args.windows(2).any(|pair| pair == ["-x", "yuv444p"]));
        assert!(args.windows(2).any(|pair| pair == ["-p", "crf=10"]));
        assert!(args.windows(2).any(|pair| pair == ["-r", "60"]));
    }

    #[test]
    fn compact_profile_uses_ui_tuned_hevc() {
        let args = recorder_args(
            Path::new("demo.mp4"),
            &CaptureTarget::Output("eDP-1".into()),
            false,
            Quality::Compact,
        );
        let args = args_as_strings(&args);

        assert!(args.windows(2).any(|pair| pair == ["-c", "libx265"]));
        assert!(args.windows(2).any(|pair| pair == ["-x", "yuv444p"]));
        assert!(args.windows(2).any(|pair| pair == ["-p", "crf=12"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-p", "preset=ultrafast"])
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["-p", "x265-params=psy-rd=0:cbqpoffs=0:crqpoffs=0",] })
        );
    }

    #[test]
    fn cli_accepts_the_supported_flags() {
        let cli = Cli::try_parse_from([
            "hyprrec",
            "--region",
            "--audio",
            "--dir",
            "/tmp/recordings",
            "--name",
            "demo",
            "--quality",
            "ultra",
            "--telemetry",
            "click,scroll,workspace-changed",
        ])
        .unwrap();

        assert!(cli.region);
        assert!(cli.audio);
        assert_eq!(cli.dir, Some(PathBuf::from("/tmp/recordings")));
        assert_eq!(cli.name.as_deref(), Some("demo"));
        assert_eq!(cli.quality, Quality::Ultra);
        assert_eq!(
            cli.telemetry,
            [
                TelemetryKind::Click,
                TelemetryKind::Scroll,
                TelemetryKind::WorkspaceChanged,
            ]
        );
    }

    #[test]
    fn high_quality_is_the_default() {
        let cli = Cli::try_parse_from(["hyprrec"]).unwrap();
        assert_eq!(cli.quality, Quality::High);
    }

    #[test]
    fn cli_accepts_compact_quality() {
        let cli = Cli::try_parse_from(["hyprrec", "--quality", "compact"]).unwrap();
        assert_eq!(cli.quality, Quality::Compact);
    }

    #[test]
    fn telemetry_sidecar_uses_recording_basename() {
        assert_eq!(
            telemetry_path(Path::new("/videos/demo.mp4")),
            Path::new("/videos/demo.telemetry.jsonl")
        );
    }
}
