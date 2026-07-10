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
4. The selected quality profile is translated into explicit `wf-recorder` codec, pixel-format, frame-rate, and codec parameters.
5. The recorder runs in the foreground. Both processes receive Ctrl+C, allowing `wf-recorder` to finalize the container while `hyprrec` waits for it.

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

The test suite covers CLI parsing, safe output naming, focused-monitor discovery, collision handling, and the exact recorder arguments for every quality profile.
