# Home Manager module: installs Montage, defines the gpu-screen-recorder user
# service, and gives config.toml its first values. After that, the file
# belongs to Montage's settings page: a rebuild never overwrites it.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.montage;
  rec' = cfg.recorder;
  inherit (lib) mkEnableOption mkIf mkOption types;

  package = cfg.package.override { gpu-screen-recorder = rec'.package; };

  initialConfig = (pkgs.formats.toml { }).generate "montage-config.toml" {
    track_names = cfg.trackNames;
    recorder = {
      monitor = rec'.monitor;
      fps = rec'.fps;
      codec = rec'.codec;
      bitrate_kbps = rec'.bitrateKbps;
      replay_seconds = rec'.replaySeconds;
      replay_storage = rec'.replayStorage;
      replay_dir = rec'.replayDir;
      record_dir = rec'.recordDir;
      audio = rec'.audio;
    };
    share = {
      max_bytes = cfg.share.maxBytes;
      fps = cfg.share.fps;
      audio_kbps = cfg.share.audioKbps;
      min_video_kbps = cfg.share.minVideoKbps;
      hd_video_kbps = cfg.share.hdVideoKbps;
      max_video_kbps = cfg.share.maxVideoKbps;
    };
    sounds = {
      enabled = cfg.sounds.enable;
      volume = cfg.sounds.volume;
    };
  };

  positive = default: description: mkOption {
    type = types.ints.positive;
    inherit default description;
  };
in
{
  options.programs.montage = {
    enable = mkEnableOption "Montage, a clip editor for the replay buffer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The Montage package.";
    };

    recorder = {
      enable = mkEnableOption "the gpu-screen-recorder user service, run through `montage recorder run`";

      package = mkOption {
        type = types.package;
        default = pkgs.gpu-screen-recorder;
        description = "The gpu-screen-recorder package, also used for `gsr-cli`.";
      };

      monitor = mkOption {
        type = types.str;
        default = "screen";
        description = "The output captured: a connector like `DP-5`, or `screen` for the first one.";
      };
      fps = positive 60 "Capture frame rate.";
      codec = mkOption {
        type = types.enum [
          "hevc"
          "h264"
          "av1"
        ];
        default = "hevc";
        description = "Capture codec.";
      };
      bitrateKbps = positive 60000 "Constant capture bitrate, in kbit/s.";
      replaySeconds = positive 60 "Length of the replay buffer, and the cap of `montage save N`.";
      replayStorage = mkOption {
        type = types.enum [
          "ram"
          "disk"
        ];
        default = "ram";
        description = "Where the replay buffer is kept.";
      };
      replayDir = mkOption {
        type = types.str;
        default = "${config.home.homeDirectory}/Videos/Replays";
        description = "Where saved replays go.";
      };
      recordDir = mkOption {
        type = types.str;
        default = "${config.home.homeDirectory}/Videos/Recordings";
        description = "Where recordings go.";
      };
      audio = mkOption {
        type = types.listOf types.str;
        default = [
          "default_output"
          "default_input"
        ];
        description = ''
          One entry per audio track, in order, in gpu-screen-recorder's `-a` syntax.
          Montage's sounds are always left out of tracks that record the desktop.
        '';
      };
    };

    share = {
      maxBytes = positive (20 * 1024 * 1024) "Size budget of the Share export, in bytes.";
      fps = positive 60 "Frame rate cap of the Share export.";
      audioKbps = positive 128 "AAC bitrate of the Share export, in kbit/s.";
      minVideoKbps = positive 2000 "Lowest video bitrate; a clip needing less is cut instead (a replay keeps its end, a recording its start).";
      hdVideoKbps = positive 4500 "From this video bitrate up, the export is 1080p instead of 720p.";
      maxVideoKbps = positive 8000 "Highest video bitrate, however short the clip.";
    };

    sounds = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Play a sound when a recording starts or a clip is saved.";
      };
      volume = mkOption {
        type = types.numbers.between 0 1;
        default = 0.6;
        description = "Volume of the sounds.";
      };
    };

    trackNames = mkOption {
      type = types.listOf types.str;
      default = [
        "Game"
        "Mic"
      ];
      description = "Names of the audio tracks in the editor, in order.";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ package ];

    # Written once: afterwards the settings page owns the file.
    home.activation.montageConfig = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      montage_config="${config.xdg.configHome}/montage/config.toml"
      if [ ! -e "$montage_config" ]; then
        run mkdir -p "$(dirname "$montage_config")"
        run install -m 644 ${initialConfig} "$montage_config"
      fi
    '';

    systemd.user.services.gpu-screen-recorder = mkIf rec'.enable {
      Unit = {
        Description = "gpu-screen-recorder replay buffer, set up by Montage";
        PartOf = [ "graphical-session.target" ];
        After = [ "pipewire.service" ];
      };
      Service = {
        # Reads config.toml, creates the folders and execs gpu-screen-recorder,
        # so the service supervises the recorder directly.
        ExecStart = "${lib.getExe package} recorder run";
        ExecStopPost = "${pkgs.coreutils}/bin/rm -f %t/gsr-recording";
        # It ignores SIGTERM.
        KillSignal = "SIGINT";
        TimeoutStopSec = 5;
        Restart = "on-failure";
        RestartSec = 5;
        Environment = [ "PATH=/run/wrappers/bin:${lib.makeBinPath [ rec'.package ]}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
