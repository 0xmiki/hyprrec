use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use chrono::Local;
use directories::UserDirs;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

const INSTRUCTIONS: &str = "Control high-quality Hyprland screen recordings. Start returns immediately. Keep the returned session_id and use it to stop the recording so the MP4 is finalized. Use recording_status to verify the active session and recording_inspect after stopping.";

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Quality {
    #[default]
    High,
    Ultra,
    Compact,
}

impl Quality {
    fn as_arg(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Ultra => "ultra",
            Self::Compact => "compact",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CaptureTarget {
    #[default]
    FocusedMonitor,
    Region,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TelemetryKind {
    Click,
    Scroll,
    KeyboardInput,
    WorkspaceChanged,
    WindowFocused,
}

impl TelemetryKind {
    fn as_arg(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Scroll => "scroll",
            Self::KeyboardInput => "keyboard-input",
            Self::WorkspaceChanged => "workspace-changed",
            Self::WindowFocused => "window-focused",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StartParams {
    /// Recording folder. Omit for $XDG_VIDEOS_DIR/hyprrec.
    dir: Option<PathBuf>,
    /// Output basename. The .mp4 extension is optional.
    name: Option<String>,
    /// Encoder profile.
    #[serde(default)]
    quality: Quality,
    /// Capture the default PipeWire audio source.
    #[serde(default)]
    audio: bool,
    /// Focused monitor or interactive region selection.
    #[serde(default)]
    target: CaptureTarget,
    /// Action telemetry categories written to a JSONL sidecar.
    #[serde(default)]
    telemetry: Vec<TelemetryKind>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionParams {
    /// Session identifier returned by recording_start.
    session_id: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListParams {
    /// Recording folder. Omit for $XDG_VIDEOS_DIR/hyprrec.
    dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectParams {
    /// MP4 recording to inspect with ffprobe.
    path: PathBuf,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct SessionInfo {
    session_id: String,
    state: String,
    pid: u32,
    video_path: PathBuf,
    telemetry_path: Option<PathBuf>,
    started_at: String,
    quality: Quality,
    audio: bool,
    target: CaptureTarget,
    exit_code: Option<i32>,
    error: Option<String>,
    diagnostic_log_path: Option<PathBuf>,
}

struct ActiveSession {
    info: SessionInfo,
    child: Child,
    diagnostic_log_path: PathBuf,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecordingEntry {
    path: PathBuf,
    size_bytes: u64,
    telemetry_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecordingList {
    dir: PathBuf,
    recordings: Vec<RecordingEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Inspection {
    path: PathBuf,
    size_bytes: u64,
    telemetry_path: Option<PathBuf>,
    probe: Value,
}

#[derive(Debug)]
struct McpError(String);

impl std::fmt::Display for McpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone)]
struct HyprrecMcp {
    active: std::sync::Arc<Mutex<Option<ActiveSession>>>,
    tool_router: ToolRouter<Self>,
}

impl HyprrecMcp {
    fn new() -> Self {
        Self {
            active: std::sync::Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl HyprrecMcp {
    #[tool(
        description = "Start a Hyprland recording and return immediately with its session ID and output paths. Only one recording may be active.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<SessionInfo>()
    )]
    async fn recording_start(&self, Parameters(params): Parameters<StartParams>) -> CallToolResult {
        structured(self.start(params).await)
    }

    #[tool(
        description = "Gracefully stop the active recording by session ID and wait for the MP4 to finalize.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<SessionInfo>()
    )]
    async fn recording_stop(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> CallToolResult {
        structured(self.stop(params).await)
    }

    #[tool(
        description = "Return the current recording session and whether its process is still running.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<Option<SessionInfo>>()
    )]
    async fn recording_status(&self) -> CallToolResult {
        structured(self.status().await)
    }

    #[tool(
        description = "List MP4 recordings and matching telemetry sidecars in a recording folder.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<RecordingList>()
    )]
    async fn recording_list(&self, Parameters(params): Parameters<ListParams>) -> CallToolResult {
        structured(list_recordings(params))
    }

    #[tool(
        description = "Inspect a completed recording with ffprobe and return stream, format, file-size, and telemetry information.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<Inspection>()
    )]
    async fn recording_inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> CallToolResult {
        structured(inspect_recording(params).await)
    }
}

impl HyprrecMcp {
    async fn start(&self, params: StartParams) -> Result<SessionInfo, McpError> {
        let mut active = self.active.lock().await;
        if let Some(session) = active.as_mut()
            && session.child.try_wait().map_err(io_error)?.is_none()
        {
            return Err(McpError(format!(
                "recording {} is already active",
                session.info.session_id
            )));
        }
        *active = None;

        let dir = resolve_dir(params.dir)?;
        fs::create_dir_all(&dir).map_err(io_error)?;
        let basename = normalize_name(params.name)?;
        let video_path = dir.join(format!("{basename}.mp4"));
        if video_path.exists() {
            return Err(McpError(format!(
                "refusing to overwrite {}",
                video_path.display()
            )));
        }
        let telemetry_path =
            (!params.telemetry.is_empty()).then(|| video_path.with_extension("telemetry.jsonl"));
        if telemetry_path.as_ref().is_some_and(|path| path.exists()) {
            return Err(McpError("telemetry sidecar already exists".into()));
        }

        let session_id = format!("rec_{}", Uuid::new_v4().simple());
        let diagnostic_log_path = dir.join(format!(".{basename}.{session_id}.log"));
        let diagnostic_log = File::create(&diagnostic_log_path).map_err(io_error)?;
        let diagnostic_stderr = diagnostic_log.try_clone().map_err(io_error)?;

        let executable = std::env::var_os("HYPRREC_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("hyprrec"));
        let mut command = Command::new(executable);
        command
            .arg("--dir")
            .arg(&dir)
            .arg("--name")
            .arg(&basename)
            .arg("--quality")
            .arg(params.quality.as_arg())
            .stdin(Stdio::null())
            .stdout(Stdio::from(diagnostic_log))
            .stderr(Stdio::from(diagnostic_stderr));
        if params.audio {
            command.arg("--audio");
        }
        if matches!(params.target, CaptureTarget::Region) {
            command.arg("--region");
        }
        if !params.telemetry.is_empty() {
            command.arg("--telemetry").arg(
                params
                    .telemetry
                    .iter()
                    .map(|kind| kind.as_arg())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        command.process_group(0);
        let child = command.spawn().map_err(|error| {
            let _ = fs::remove_file(&diagnostic_log_path);
            io_error(error)
        })?;
        let pid = child
            .id()
            .ok_or_else(|| McpError("recording process has no PID".into()))?;
        let info = SessionInfo {
            session_id,
            state: "recording".into(),
            pid,
            video_path,
            telemetry_path,
            started_at: chrono::Utc::now().to_rfc3339(),
            quality: params.quality,
            audio: params.audio,
            target: params.target,
            exit_code: None,
            error: None,
            diagnostic_log_path: Some(diagnostic_log_path.clone()),
        };
        *active = Some(ActiveSession {
            info: info.clone(),
            child,
            diagnostic_log_path,
        });
        Ok(info)
    }

    async fn stop(&self, params: SessionParams) -> Result<SessionInfo, McpError> {
        let mut session = self
            .active
            .lock()
            .await
            .take()
            .ok_or_else(|| McpError("no recording session exists".into()))?;
        if session.info.session_id != params.session_id {
            let expected = session.info.session_id.clone();
            *self.active.lock().await = Some(session);
            return Err(McpError(format!(
                "unknown session ID; active session is {expected}"
            )));
        }

        if session.child.try_wait().map_err(io_error)?.is_none() {
            let result = unsafe { libc::kill(-(session.info.pid as i32), libc::SIGINT) };
            if result != 0 {
                let error = McpError(format!(
                    "could not signal recorder: {}",
                    std::io::Error::last_os_error()
                ));
                *self.active.lock().await = Some(session);
                return Err(error);
            }
        }

        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = session.child.try_wait().map_err(io_error)? {
                break status;
            }
            if Instant::now() >= deadline {
                *self.active.lock().await = Some(session);
                return Err(McpError(
                    "recorder did not finalize within 20 seconds; session remains active".into(),
                ));
            }
            sleep(Duration::from_millis(100)).await;
        };
        finish_info(&mut session.info, status, &session.diagnostic_log_path);
        Ok(session.info)
    }

    async fn status(&self) -> Result<Option<SessionInfo>, McpError> {
        let mut active = self.active.lock().await;
        let Some(session) = active.as_mut() else {
            return Ok(None);
        };
        if let Some(status) = session.child.try_wait().map_err(io_error)? {
            finish_info(&mut session.info, status, &session.diagnostic_log_path);
        }
        Ok(Some(session.info.clone()))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HyprrecMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "hyprrec-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = HyprrecMcp::new()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| anyhow::anyhow!("MCP initialization failed: {error}"))?;
    service
        .waiting()
        .await
        .map_err(|error| anyhow::anyhow!("MCP server stopped: {error}"))?;
    Ok(())
}

fn structured<T: Serialize>(result: Result<T, McpError>) -> CallToolResult {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => tool_error(error.to_string()),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

fn tool_error(message: String) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "error": {"message": message}
    }))
}

fn resolve_dir(dir: Option<PathBuf>) -> Result<PathBuf, McpError> {
    if let Some(dir) = dir {
        return Ok(dir);
    }
    let dirs = UserDirs::new().ok_or_else(|| McpError("could not find home directory".into()))?;
    Ok(dirs
        .video_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.home_dir().join("Videos"))
        .join("hyprrec"))
}

fn normalize_name(name: Option<String>) -> Result<String, McpError> {
    let name = name.unwrap_or_else(|| format!("hyprrec-{}", Local::now().format("%Y%m%d-%H%M%S")));
    let name = name.trim().strip_suffix(".mp4").unwrap_or(name.trim());
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(McpError("name must be a non-empty basename".into()));
    }
    Ok(name.to_owned())
}

fn finish_info(info: &mut SessionInfo, status: ExitStatus, diagnostic_log_path: &Path) {
    info.state = if status.success() {
        "completed".into()
    } else {
        "failed".into()
    };
    info.exit_code = status.code();
    if status.success() {
        let _ = fs::remove_file(diagnostic_log_path);
        info.diagnostic_log_path = None;
        info.error = None;
    } else {
        info.error = read_diagnostic_tail(diagnostic_log_path);
    }
}

fn read_diagnostic_tail(path: &Path) -> Option<String> {
    const MAX_BYTES: usize = 16 * 1024;
    let bytes = fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(MAX_BYTES);
    let message = String::from_utf8_lossy(&bytes[start..]).trim().to_owned();
    (!message.is_empty()).then_some(message)
}

fn list_recordings(params: ListParams) -> Result<RecordingList, McpError> {
    let dir = resolve_dir(params.dir)?;
    let mut recordings = Vec::new();
    if dir.exists() {
        for entry in fs::read_dir(&dir).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("mp4") {
                continue;
            }
            let size_bytes = entry.metadata().map_err(io_error)?.len();
            let sidecar = path.with_extension("telemetry.jsonl");
            recordings.push(RecordingEntry {
                path,
                size_bytes,
                telemetry_path: sidecar.exists().then_some(sidecar),
            });
        }
    }
    recordings.sort_by(|left, right| right.path.cmp(&left.path));
    Ok(RecordingList { dir, recordings })
}

async fn inspect_recording(params: InspectParams) -> Result<Inspection, McpError> {
    let metadata = fs::metadata(&params.path).map_err(io_error)?;
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(&params.path)
        .output()
        .await
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(McpError(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let probe = serde_json::from_slice(&output.stdout)
        .map_err(|error| McpError(format!("invalid ffprobe JSON: {error}")))?;
    let sidecar = params.path.with_extension("telemetry.jsonl");
    Ok(Inspection {
        path: params.path,
        size_bytes: metadata.len(),
        telemetry_path: sidecar.exists().then_some(sidecar),
        probe,
    })
}

fn io_error(error: std::io::Error) -> McpError {
    McpError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_explicit_and_generated_names() {
        assert_eq!(normalize_name(Some("demo.mp4".into())).unwrap(), "demo");
        assert!(normalize_name(None).unwrap().starts_with("hyprrec-"));
    }

    #[test]
    fn rejects_names_containing_paths() {
        assert!(normalize_name(Some("../demo".into())).is_err());
        assert!(normalize_name(Some("folder/demo".into())).is_err());
    }

    #[test]
    fn lists_video_and_matching_telemetry() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("demo.mp4"), b"video").unwrap();
        fs::write(temp.path().join("demo.telemetry.jsonl"), b"{}\n").unwrap();
        fs::write(temp.path().join("ignore.txt"), b"text").unwrap();

        let result = list_recordings(ListParams {
            dir: Some(temp.path().to_path_buf()),
        })
        .unwrap();

        assert_eq!(result.recordings.len(), 1);
        assert_eq!(result.recordings[0].size_bytes, 5);
        assert!(result.recordings[0].telemetry_path.is_some());
    }
}
