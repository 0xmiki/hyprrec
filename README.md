# hyprrec

`hyprrec` is a small Rust CLI for high-quality screen recording on Hyprland. It records the focused monitor or an interactively selected region through `wf-recorder`, optionally captures PipeWire audio, and writes timestamped MP4 files to your Videos directory.

## Usage

Once installed through NixOS, show every available option with:

```console
hyprrec --help
```

Record the focused monitor with the default profile:

```console
hyprrec
```

Press <kbd>Ctrl</kbd>+<kbd>C</kbd> to stop. `wf-recorder` will finalize the MP4 before `hyprrec` exits.

Common examples:

```console
# Select a region interactively
hyprrec --region

# Include the default PipeWire audio source
hyprrec --audio

# Record a high-fidelity editing master
hyprrec --region --audio --quality ultra --name product-demo

# Record directly to a smaller HEVC file
hyprrec --quality compact

# Choose a different destination
hyprrec --dir ~/Recordings --name walkthrough
```

By default, recordings are stored in `$XDG_VIDEOS_DIR/hyprrec`, normally `~/Videos/hyprrec`, with names such as `hyprrec-20260710-110547.mp4`. `--name` accepts a basename, adds `.mp4` automatically, and refuses path separators or overwriting an existing file.

### Codex MCP server

The package also installs `hyprrec-mcp`, a stdio MCP server that lets Codex manage recordings without constructing shell commands. It exposes:

| Tool | Purpose |
| --- | --- |
| `recording_start` | Start a focused-monitor or interactive-region recording and immediately return its session ID and artifact paths |
| `recording_stop` | Stop a session gracefully and wait for the MP4 to finalize |
| `recording_status` | Report the current session and process state |
| `recording_list` | List recordings and telemetry sidecars in the default or supplied directory |
| `recording_inspect` | Return FFprobe stream, format, size, and sidecar information |

`recording_start` accepts `dir`, `name`, `quality`, `audio`, `target`, and `telemetry`. For example, Codex can request an ultra-quality focused-monitor recording in a project-specific folder with all action telemetry enabled.

After installing the Nix package, register the server globally with Codex:

```console
codex mcp add hyprrec -- /run/current-system/sw/bin/hyprrec-mcp
codex mcp list
```

Then ensure the server inherits the active Hyprland variables in `~/.codex/config.toml`:

```toml
[mcp_servers.hyprrec]
command = "/run/current-system/sw/bin/hyprrec-mcp"
env_vars = ["HYPRLAND_INSTANCE_SIGNATURE", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"]
```

Start a new Codex session after adding or changing the server. For a development build, replace the installed path with `target/release/hyprrec-mcp`. These inherited variables let the recorder reach the active compositor and remain correct when the Hyprland instance signature changes.

### Action telemetry

Telemetry is disabled unless `--telemetry` is supplied. Categories can be comma-separated or supplied by repeating the flag:

```console
hyprrec --quality ultra \
  --telemetry click,scroll,keyboard-input,workspace-changed,window-focused
```

This creates a sidecar with the same basename as the video:

```text
product-demo.mp4
product-demo.telemetry.jsonl
```

Available categories:

| Category | Events |
| --- | --- |
| `click` | Pressed pointer button, button name/code and Hyprland cursor position |
| `scroll` | Throttled horizontal/vertical scroll deltas from mouse wheels or touchpad fingers |
| `keyboard-input` | Named control keys and shortcuts; printable typing is recorded only as `text` activity |
| `workspace-changed` | Hyprland workspace ID/name changes |
| `window-focused` | Focused Hyprland window class and title |

Each JSONL event includes a schema version, sequence number, milliseconds relative to telemetry start, wall-clock milliseconds, source, event type and structured data:

```json
{"schema_version":1,"sequence":12,"t_ms":2410,"unix_ms":1783680012410,"source":"libinput","type":"pointer.click","data":{"button":"left","button_code":272,"cursor":{"x":1106,"y":102}}}
```

Input telemetry uses `libinput` and requires the recording user to belong to the system `input` group. Keyboard telemetry never stores ordinary printable characters, form contents or reconstructed text. Shortcuts retain their key name so an editor can distinguish actions such as Ctrl+C from normal typing. Window titles can contain private document or page names, so enable `window-focused` only when appropriate for the recording.

### Quality profiles

| Profile | Video encoding | Best use | Tradeoff |
| --- | --- | --- | --- |
| `high` (default) | H.264, 4:2:0, CRF 18, 60 fps | Everyday recordings and broad playback compatibility | Slightly less color detail around fine UI edges |
| `ultra` | H.264, 4:4:4, CRF 10, 60 fps | Product-demo masters and editing | Large files and reduced hardware-decoder compatibility |
| `compact` | HEVC, 4:4:4, CRF 12, 60 fps | Smaller files with visually near-ultra quality | Lower player/editor compatibility; technically still lossy |

For the safest product-demo workflow, record and edit with `ultra`, then compress the final cut. Keep the ultra master until the delivered file has been reviewed:

```console
ffmpeg -i input.mp4 \
  -c:v libx265 -preset medium -crf 12 -pix_fmt yuv444p \
  -x265-params 'psy-rd=0:cbqpoffs=0:crqpoffs=0' \
  -tag:v hvc1 -c:a copy output-compact.mp4
```

The conversion is designed to be visually indistinguishable in normal viewing, but no lossy codec can provide a materially smaller file with literally zero information loss.

## How it works

1. The CLI verifies that it is running in a Hyprland Wayland session and that its runtime commands are available.
2. For full-screen capture, `hyprctl -j monitors` identifies the focused active output.
3. With `--region`, `slurp` returns the selected geometry; cancelling selection exits cleanly.
4. Requested telemetry collectors write an independent, timestamped JSONL sidecar.
5. The selected quality profile is translated into explicit `wf-recorder` codec, pixel-format, frame-rate, and codec parameters.
6. The recorder runs in the foreground. Both processes receive Ctrl+C, allowing `wf-recorder` to finalize the container while `hyprrec` waits for it.

Audio is disabled by default. `--audio` records the current default PipeWire source; selecting a specific audio device is not currently supported.

## NixOS installation

The repository includes [`package.nix`](./package.nix), which builds the Rust release binary and wraps it so `wf-recorder`, `slurp`, and `hyprctl` are always on its runtime `PATH`.

Import it from `configuration.nix`:

```nix
{ config, pkgs, ... }:

let
  hyprrec = pkgs.callPackage /path/to/hyprrec/package.nix {};
in
{
  environment.systemPackages = with pkgs; [
    hyprrec
  ];
}
```

Then apply the configuration:

```console
sudo nixos-rebuild switch
```

This computer currently keeps its system-facing copy at `/home/miki/packages/hyprrec/package.nix` and imports it from `/home/miki/dotfiles/configuration.nix`. The in-repository version is portable and uses the current project directory as its source.

After changing Rust code or `Cargo.lock`, rebuilding NixOS creates a new package derivation automatically.

## Development

Enter the provided development environment:

```console
nix-shell
```

Run the checks:

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Run directly from the project without installing:

```console
cargo run --release -- --help
cargo run --release -- --region --quality ultra
```

The test suite covers CLI parsing, safe output naming, focused-monitor discovery, collision handling, privacy-safe key classification, telemetry sidecar naming, and the exact recorder arguments for every quality profile.
