{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  libinput,
  wf-recorder,
  slurp,
  hyprland,
}:

let
  source = lib.cleanSourceWith {
    src = ./.;
    filter = path: type:
      let
        name = baseNameOf path;
      in
      name != "target" && name != ".git";
  };
in
rustPlatform.buildRustPackage {
  pname = "hyprrec";
  version = "0.1.0";

  src = source;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ makeWrapper pkg-config ];
  buildInputs = [ libinput ];

  postInstall = ''
    wrapProgram $out/bin/hyprrec \
      --prefix PATH : ${lib.makeBinPath [ wf-recorder slurp hyprland ]}
  '';

  meta = {
    description = "High-quality screen recording CLI for Hyprland";
    mainProgram = "hyprrec";
    platforms = lib.platforms.linux;
  };
}
