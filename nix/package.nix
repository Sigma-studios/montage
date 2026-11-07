{
  lib,
  rustPlatform,
  pkg-config,
  ffmpeg,
  alsa-lib,
  wayland,
  libxkbcommon,
  vulkan-loader,
  libGL,
  xorg,
  gpu-screen-recorder,
  pipewire,
  systemd,
}:

rustPlatform.buildRustPackage {
  pname = "montage";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../assets
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
  buildInputs = [
    ffmpeg
    alsa-lib
  ];

  # The tests render the UI with wgpu and need a GPU and fonts, which the
  # build sandbox lacks; CI runs them.
  doCheck = false;

  # The programs the recorder controls run, baked in so key bindings work
  # whatever PATH the compositor has. gpu-screen-recorder itself is found
  # through the service's PATH, so a capability wrapper in
  # /run/wrappers/bin wins.
  env = {
    MONTAGE_GSR_CLI = "${gpu-screen-recorder}/bin/gsr-cli";
    MONTAGE_PW_PLAY = "${pipewire}/bin/pw-play";
    MONTAGE_SYSTEMD_RUN = "${systemd}/bin/systemd-run";
  };

  postInstall = ''
    install -Dm644 assets/montage.desktop $out/share/applications/montage.desktop
  '';

  # winit and wgpu load these at run time with dlopen.
  postFixup = ''
    patchelf --add-rpath ${
      lib.makeLibraryPath [
        wayland
        libxkbcommon
        vulkan-loader
        libGL
        xorg.libX11
        xorg.libXcursor
        xorg.libXi
        xorg.libXrandr
      ]
    } $out/bin/montage
  '';

  meta = {
    description = "A clip editor for the replay buffer";
    homepage = "https://github.com/Sigma-studios/montage";
    license = lib.licenses.agpl3Only;
    mainProgram = "montage";
    platforms = lib.platforms.linux;
  };
}
