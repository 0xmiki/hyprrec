{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  packages = with pkgs; [
    # Rust toolchain
    rustc
    cargo
    rustfmt
    clippy

    # Initial recording backend
    wf-recorder
    slurp

    # Useful development tools
    pkg-config
  ];

  env = {
    RUST_BACKTRACE = "1";
    RUST_LOG = "hyprrec=debug";
  };

  shellHook = ''
    echo "🦀 hyprrec development shell"
    echo "Rust: $(rustc --version)"
    echo "Cargo: $(cargo --version)"
    echo ""
    echo "Available recording tools:"
    echo "  wf-recorder: $(command -v wf-recorder)"
    echo "  slurp:       $(command -v slurp)"
    echo "  ffmpeg:      $(command -v ffmpeg)"
  '';
}