# Montage

A clip editor for the replay buffer, for Linux, Windows and macOS. Open one
saved clip, cut it, mute or adjust each audio track, add text, and export it
to the clipboard or a file, as MP4 or GIF.

On Linux it also runs gpu-screen-recorder: starting and stopping recordings,
saving the replay buffer, the sounds, and sharing each new clip as a small MP4
on the clipboard. The recorder's settings live in Montage's `config.toml`, and
a Home Manager module sets up the service.

**Status:** every phase is written and tested on macOS (171 tests); nothing
has run on Linux or Windows yet. See "Where it stands" in
[the design](docs/design.md), and [the recorder appendix](docs/recorder.md).

```
montage                         the library
montage open <clip>             the editor
montage settings                the settings page
montage share <clip> [--type replay|regular|screenshot]
montage export <clip> --preset share|full|gif [--out <path>] [--clipboard]

Linux:
montage record start|stop|toggle
montage save [N] [edit]
montage on-save <file> <type>   the recorder's hook
montage recorder run            what the service runs
```

## Building

Needs FFmpeg's libraries (7.1 or newer) and `pkg-config`: `brew install ffmpeg
pkg-config` on macOS, the `libav*-dev` packages and `libasound2-dev` on Debian
and Ubuntu, BtbN's shared GPL build with `FFMPEG_DIR` set on Windows, or `nix
build` / `nix develop`.

`MONTAGE_HWDEC=0` forces software decoding in the preview, and
`MONTAGE_NO_AUDIO=1` plays without sound.

On Linux, with Home Manager:

```nix
inputs.montage.url = "github:Sigma-studios/montage";
# then, in the home configuration
imports = [ inputs.montage.homeManagerModules.default ];
programs.montage = { enable = true; recorder.enable = true; };
```

## Layout

- `crates/montage-core`: clip folders, `montage.toml`, `config.toml`, the
  recorder's command line and the Share rules. No libav, GPU or desktop code,
  so it builds and tests the same on every system.
- `crates/montage-media`: libav: probing, the player (GPU decode, cpal audio),
  the filmstrip, waveform and thumbnails, and the Share, Full quality and GIF
  exports.
- `crates/montage-text`: captions: fonts, and text with a stroked outline.
- `crates/montage`: the binary: the commands, the recorder controls, the
  window (`src/ui/`: library, editor, settings), and what differs per system
  in `src/platform/`.
- `nix/`: the package and the Home Manager module.

## License

AGPL-3.0, as gifski, which Montage uses for GIFs, requires.
