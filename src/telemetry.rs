use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use input::event::Event;
use input::event::keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait};
use input::event::pointer::{Axis, ButtonState, PointerEvent, PointerScrollEvent};
use input::{Libinput, LibinputInterface};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SCHEMA_VERSION: u8 = 1;
const SCROLL_EMIT_INTERVAL: Duration = Duration::from_millis(33);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryKind {
    Click,
    Scroll,
    KeyboardInput,
    WorkspaceChanged,
    WindowFocused,
}

pub struct TelemetrySession {
    shared: Arc<SharedWriter>,
    path: PathBuf,
}

struct SharedWriter {
    started: Instant,
    sequence: AtomicU64,
    running: AtomicBool,
    writer: Mutex<BufWriter<File>>,
}

#[derive(Serialize)]
struct EventEnvelope<'a> {
    schema_version: u8,
    sequence: u64,
    t_ms: u128,
    unix_ms: u128,
    source: &'a str,
    #[serde(rename = "type")]
    event_type: &'a str,
    data: Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct CursorPosition {
    x: i64,
    y: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ActiveWindow {
    #[serde(default)]
    address: String,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
}

impl TelemetrySession {
    pub fn start(video_path: &Path, requested: &[TelemetryKind]) -> io::Result<Self> {
        let path = video_path.with_extension("telemetry.jsonl");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let shared = Arc::new(SharedWriter {
            started: Instant::now(),
            sequence: AtomicU64::new(0),
            running: AtomicBool::new(true),
            writer: Mutex::new(BufWriter::new(file)),
        });
        let enabled: HashSet<_> = requested.iter().copied().collect();

        shared.emit(
            "hyprrec",
            "telemetry.started",
            json!({
                "video": video_path,
                "categories": requested,
                "keyboard_privacy": "printable keys are recorded as text activity without characters",
            }),
        )?;

        let startup = (|| {
            if enabled.contains(&TelemetryKind::Click)
                || enabled.contains(&TelemetryKind::Scroll)
                || enabled.contains(&TelemetryKind::KeyboardInput)
            {
                spawn_libinput_collector(Arc::clone(&shared), enabled.clone())?;
            }

            if enabled.contains(&TelemetryKind::WorkspaceChanged)
                || enabled.contains(&TelemetryKind::WindowFocused)
            {
                spawn_hyprland_collector(Arc::clone(&shared), enabled)?;
            }
            Ok(())
        })();

        if let Err(error) = startup {
            shared.running.store(false, Ordering::SeqCst);
            drop(shared);
            let _ = fs::remove_file(&path);
            return Err(error);
        }

        Ok(Self { shared, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stop(&self, recording_succeeded: bool) {
        if !self.shared.running.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.shared.emit(
            "hyprrec",
            "telemetry.stopped",
            json!({ "recording_succeeded": recording_succeeded }),
        ) {
            eprintln!("hyprrec: could not finalize telemetry: {error}");
        }
    }
}

impl Drop for TelemetrySession {
    fn drop(&mut self) {
        self.stop(false);
    }
}

impl SharedWriter {
    fn emit(&self, source: &str, event_type: &str, data: Value) -> io::Result<()> {
        let envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            t_ms: self.started.elapsed().as_millis(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            source,
            event_type,
            data,
        };

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("telemetry writer lock was poisoned"))?;
        serde_json::to_writer(&mut *writer, &envelope)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }
}

struct InputInterface;

impl LibinputInterface for InputInterface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read(
                (flags & libc::O_ACCMODE == libc::O_RDONLY)
                    | (flags & libc::O_ACCMODE == libc::O_RDWR),
            )
            .write(
                (flags & libc::O_ACCMODE == libc::O_WRONLY)
                    | (flags & libc::O_ACCMODE == libc::O_RDWR),
            )
            .open(path)
            .map(Into::into)
            .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

fn spawn_libinput_collector(
    shared: Arc<SharedWriter>,
    enabled: HashSet<TelemetryKind>,
) -> io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hyprrec-libinput".into())
        .spawn(move || {
            let mut input = Libinput::new_with_udev(InputInterface);
            if input.udev_assign_seat("seat0").is_err() {
                let _ = sender.send(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "could not access seat0 input devices; ensure the user belongs to the input group",
                )));
                return;
            }
            if sender.send(Ok(())).is_ok() {
                collect_libinput(&mut input, &shared, &enabled);
            }
        })?;
    receiver
        .recv()
        .map_err(|_| io::Error::other("libinput collector exited during startup"))?
}

fn collect_libinput(
    input: &mut Libinput,
    shared: &Arc<SharedWriter>,
    enabled: &HashSet<TelemetryKind>,
) {
    let mut modifiers = HashSet::new();
    let mut scroll = ScrollAccumulator::default();

    while shared.running.load(Ordering::Relaxed) {
        if let Err(error) = input.dispatch() {
            eprintln!("hyprrec: input telemetry stopped: {error}");
            return;
        }

        for event in &mut *input {
            match event {
                Event::Pointer(PointerEvent::Button(event))
                    if enabled.contains(&TelemetryKind::Click)
                        && event.button_state() == ButtonState::Pressed =>
                {
                    emit_ignored(
                        shared,
                        "libinput",
                        "pointer.click",
                        json!({
                            "button": button_name(event.button()),
                            "button_code": event.button(),
                            "cursor": query_cursor_position(),
                        }),
                    );
                }
                Event::Pointer(PointerEvent::ScrollWheel(event))
                    if enabled.contains(&TelemetryKind::Scroll) =>
                {
                    accumulate_scroll(&mut scroll, &event, "wheel", shared);
                }
                Event::Pointer(PointerEvent::ScrollFinger(event))
                    if enabled.contains(&TelemetryKind::Scroll) =>
                {
                    accumulate_scroll(&mut scroll, &event, "finger", shared);
                }
                Event::Pointer(PointerEvent::ScrollContinuous(event))
                    if enabled.contains(&TelemetryKind::Scroll) =>
                {
                    accumulate_scroll(&mut scroll, &event, "continuous", shared);
                }
                Event::Keyboard(KeyboardEvent::Key(event))
                    if enabled.contains(&TelemetryKind::KeyboardInput) =>
                {
                    handle_keyboard(event.key(), event.key_state(), &mut modifiers, shared);
                }
                _ => {}
            }
        }

        scroll.flush_if_due(shared);
        thread::sleep(Duration::from_millis(5));
    }
    scroll.flush(shared);
}

#[derive(Default)]
struct ScrollAccumulator {
    horizontal: f64,
    vertical: f64,
    source: Option<&'static str>,
    last_emit: Option<Instant>,
}

impl ScrollAccumulator {
    fn add(&mut self, horizontal: f64, vertical: f64, source: &'static str) {
        self.horizontal += horizontal;
        self.vertical += vertical;
        self.source = Some(source);
        self.last_emit.get_or_insert_with(Instant::now);
    }

    fn flush_if_due(&mut self, shared: &SharedWriter) {
        if self
            .last_emit
            .is_some_and(|started| started.elapsed() >= SCROLL_EMIT_INTERVAL)
        {
            self.flush(shared);
        }
    }

    fn flush(&mut self, shared: &SharedWriter) {
        if self.horizontal == 0.0 && self.vertical == 0.0 {
            self.last_emit = None;
            return;
        }
        emit_ignored(
            shared,
            "libinput",
            "pointer.scroll",
            json!({
                "horizontal": self.horizontal,
                "vertical": self.vertical,
                "source": self.source.unwrap_or("unknown"),
                "cursor": query_cursor_position(),
            }),
        );
        self.horizontal = 0.0;
        self.vertical = 0.0;
        self.source = None;
        self.last_emit = None;
    }
}

fn accumulate_scroll<E: PointerScrollEvent>(
    accumulator: &mut ScrollAccumulator,
    event: &E,
    source: &'static str,
    shared: &SharedWriter,
) {
    let horizontal = event.scroll_value(Axis::Horizontal);
    let vertical = event.scroll_value(Axis::Vertical);
    if horizontal == 0.0 && vertical == 0.0 {
        accumulator.flush(shared);
    } else {
        accumulator.add(horizontal, vertical, source);
    }
}

fn handle_keyboard(
    code: u32,
    state: KeyState,
    modifiers: &mut HashSet<u32>,
    shared: &SharedWriter,
) {
    if is_modifier(code) {
        match state {
            KeyState::Pressed => {
                modifiers.insert(code);
            }
            KeyState::Released => {
                modifiers.remove(&code);
            }
        }
        return;
    }
    if state != KeyState::Pressed {
        return;
    }

    let mut modifier_names: Vec<_> = modifiers.iter().map(|code| modifier_name(*code)).collect();
    modifier_names.sort_unstable();
    let reveal_key = !modifier_names.is_empty() || is_safe_control_key(code);
    emit_ignored(
        shared,
        "libinput",
        "keyboard.input",
        json!({
            "key": if reveal_key { key_name(code) } else { "text" },
            "modifiers": modifier_names,
            "content_recorded": false,
        }),
    );
}

fn is_modifier(code: u32) -> bool {
    matches!(code, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126)
}

fn modifier_name(code: u32) -> &'static str {
    match code {
        29 | 97 => "ctrl",
        42 | 54 => "shift",
        56 | 100 => "alt",
        125 | 126 => "super",
        _ => "modifier",
    }
}

fn is_safe_control_key(code: u32) -> bool {
    matches!(
        code,
        1 | 14 | 15 | 28 | 58 | 59..=68 | 87 | 88 | 96 | 102..=111 | 113..=115
    )
}

fn key_name(code: u32) -> &'static str {
    match code {
        1 => "escape",
        2 => "1",
        3 => "2",
        4 => "3",
        5 => "4",
        6 => "5",
        7 => "6",
        8 => "7",
        9 => "8",
        10 => "9",
        11 => "0",
        14 => "backspace",
        15 => "tab",
        16 => "q",
        17 => "w",
        18 => "e",
        19 => "r",
        20 => "t",
        21 => "y",
        22 => "u",
        23 => "i",
        24 => "o",
        25 => "p",
        28 | 96 => "enter",
        30 => "a",
        31 => "s",
        32 => "d",
        33 => "f",
        34 => "g",
        35 => "h",
        36 => "j",
        37 => "k",
        38 => "l",
        44 => "z",
        45 => "x",
        46 => "c",
        47 => "v",
        48 => "b",
        49 => "n",
        50 => "m",
        58 => "caps-lock",
        59 => "f1",
        60 => "f2",
        61 => "f3",
        62 => "f4",
        63 => "f5",
        64 => "f6",
        65 => "f7",
        66 => "f8",
        67 => "f9",
        68 => "f10",
        87 => "f11",
        88 => "f12",
        102 => "home",
        103 => "up",
        104 => "page-up",
        105 => "left",
        106 => "right",
        107 => "end",
        108 => "down",
        109 => "page-down",
        110 => "insert",
        111 => "delete",
        113 => "mute",
        114 => "volume-down",
        115 => "volume-up",
        _ => "key",
    }
}

fn button_name(code: u32) -> &'static str {
    match code {
        0x110 => "left",
        0x111 => "right",
        0x112 => "middle",
        0x113 => "side",
        0x114 => "extra",
        0x115 => "forward",
        0x116 => "back",
        _ => "other",
    }
}

fn spawn_hyprland_collector(
    shared: Arc<SharedWriter>,
    enabled: HashSet<TelemetryKind>,
) -> io::Result<()> {
    let stream = UnixStream::connect(hyprland_socket(".socket2.sock")?)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    thread::Builder::new()
        .name("hyprrec-hyprland-events".into())
        .spawn(move || collect_hyprland_events(stream, &shared, &enabled))?;
    Ok(())
}

fn collect_hyprland_events(
    stream: UnixStream,
    shared: &SharedWriter,
    enabled: &HashSet<TelemetryKind>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut last_workspace = String::new();
    let mut last_window_address = String::new();
    let mut last_emitted_window_address = String::new();

    if enabled.contains(&TelemetryKind::WindowFocused)
        && let Some(window) = query_active_window()
    {
        last_window_address = window.address.clone();
        last_emitted_window_address = window.address.clone();
        emit_ignored(
            shared,
            "hyprland",
            "window.focused",
            json!({
                "address": window.address,
                "class": window.class,
                "title": window.title,
                "initial": true,
            }),
        );
    }

    while shared.running.load(Ordering::Relaxed) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                let event = line.trim_end();
                if enabled.contains(&TelemetryKind::WorkspaceChanged) {
                    if let Some(data) = event.strip_prefix("workspacev2>>") {
                        let (id, name) = data.split_once(',').unwrap_or((data, data));
                        if name != last_workspace {
                            last_workspace = name.to_owned();
                            emit_ignored(
                                shared,
                                "hyprland",
                                "workspace.changed",
                                json!({ "id": id, "name": name }),
                            );
                        }
                    } else if let Some(name) = event.strip_prefix("workspace>>")
                        && name != last_workspace
                    {
                        last_workspace = name.to_owned();
                        emit_ignored(
                            shared,
                            "hyprland",
                            "workspace.changed",
                            json!({ "name": name }),
                        );
                    }
                }

                if enabled.contains(&TelemetryKind::WindowFocused)
                    && let Some(address) = event.strip_prefix("activewindowv2>>")
                    && address != last_window_address
                {
                    last_window_address = address.to_owned();
                    if let Some(window) = query_active_window()
                        && window.address != last_emitted_window_address
                    {
                        last_emitted_window_address = window.address.clone();
                        emit_ignored(
                            shared,
                            "hyprland",
                            "window.focused",
                            json!({
                                "address": window.address,
                                "class": window.class,
                                "title": window.title,
                                "initial": false,
                            }),
                        );
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                eprintln!("hyprrec: Hyprland telemetry stopped: {error}");
                return;
            }
        }
    }
}

fn query_cursor_position() -> Option<CursorPosition> {
    serde_json::from_str(&hyprland_request("j/cursorpos")?).ok()
}

fn query_active_window() -> Option<ActiveWindow> {
    serde_json::from_str(&hyprland_request("j/activewindow")?).ok()
}

fn hyprland_request(request: &str) -> Option<String> {
    let socket = hyprland_socket(".socket.sock").ok()?;
    let mut stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn hyprland_socket(name: &str) -> io::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    let signature = env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HYPRLAND_INSTANCE_SIGNATURE is not set",
        )
    })?;
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(name))
}

fn emit_ignored(shared: &SharedWriter, source: &str, event_type: &str, data: Value) {
    if shared.running.load(Ordering::Relaxed)
        && let Err(error) = shared.emit(source, event_type, data)
    {
        eprintln!("hyprrec: could not write telemetry event: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_keys_are_hidden_without_modifiers() {
        assert!(!is_safe_control_key(30));
        assert_eq!(key_name(30), "a");
    }

    #[test]
    fn control_keys_have_safe_names() {
        assert!(is_safe_control_key(28));
        assert_eq!(key_name(28), "enter");
        assert_eq!(key_name(103), "up");
    }

    #[test]
    fn mouse_buttons_have_stable_names() {
        assert_eq!(button_name(0x110), "left");
        assert_eq!(button_name(0x111), "right");
    }

    #[test]
    fn active_window_ignores_unneeded_hyprland_fields() {
        let window: ActiveWindow = serde_json::from_str(
            r#"{"address":"0xabc","class":"Alacritty","title":"Demo","pid":42}"#,
        )
        .unwrap();
        assert_eq!(window.address, "0xabc");
        assert_eq!(window.class, "Alacritty");
        assert_eq!(window.title, "Demo");
    }
}
