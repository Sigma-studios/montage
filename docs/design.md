# montage: a clip editor for the replay buffer

2026-11-07, revised the same day to run on Linux, Windows and macOS

On Linux, Montage also runs gpu-screen-recorder's controls: see [`recorder.md`](recorder.md), which replaces this document where they disagree.

Montage opens one saved clip, lets you cut it, mute or adjust each audio source, add text, and export it to the clipboard or a file, as MP4 or GIF. It runs on Linux, Windows and macOS.

On the Linux desktop it replaces the `gsr-clip-export` shell script in nixos-config: the automatic "save the replay, copy a small MP4 to the clipboard" step becomes `montage share`, so the editor and the automatic export use the same code and give the same output. On Windows and macOS there's no recorder hook. Montage edits the clips other recorders save (NVIDIA app, OBS, the macOS screen recorder), and `montage share` can be run by hand, from "Open with", or from a recorder's own "after save" script if it has one.

## Goals and scope

**In scope**

- Editing a single clip: cut the start and end, mute or set the volume of each audio track, add text over the video, then export.
- Export as MP4 or GIF, either to the clipboard or to a file you choose. The clipboard holds the file itself, so pasting into Discord or a browser uploads it, on all three systems.
- Linux, Windows and macOS, from the same code. Platform code is limited to a few small modules listed under [Platforms](#platforms).
- Recording game and mic as separate audio tracks on the Linux recorder, so they can be adjusted separately. Montage handles any number of tracks, whichever recorder made the clip.
- `/clip edit` in Génie (Linux), which also takes a duration: `/clip 15 edit` and `/clip edit 15` both save the last 15 s and open the clip in Montage.
- A headless `montage share` that takes over from `gsr-clip-export`, with the same 20 MB rules.
- A library of saved clips, with deleting only when you ask.

**Out of scope for now**

- Joining several clips into one.
- A `/montage` command in Génie. Montage is opened by `/clip edit`, the notification, or its own desktop entry or Start menu or app bundle.
- `/clip` on its own opening the editor. It stays instant: it saves and copies.
- Watching folders on Windows and macOS to run `share` automatically.
- Deleting anything automatically. Deleting is always a click you make, and goes to the trash.
- Speed changes, transitions, effects, color.
- Uploading to a host.
- An installer, code signing and notarization. Windows gets a zip and macOS an unsigned `.app` until someone needs more.

## How it fits together

On Linux, with gpu-screen-recorder:

```
Ctrl+Alt+Insert / Génie /clip [N] [edit]
        │
        ▼
   gsr-replay [N] [edit]  ──(edit: writes $XDG_RUNTIME_DIR/montage-edit-next)──┐
        │ save-replay N                                                         │
        ▼                                                                       │
 gpu-screen-recorder ── file written ──▶ gsr-on-save (chime)                   │
                                            │                                   │
                          flag present? ────┴── yes: montage open <file> ◀──────┘
                                            │
                                            no: montage share <file>   (clipboard, notification)
```

- **Two audio tracks in every new clip:** `-a 'app-inverse:gsr-chime' -a 'default_input'` instead of the one mixed `-a`. Track 1 is the desktop (the game), track 2 is the mic. Exports always mix them down to one track, so Discord and browsers keep working. Older clips have one track, and Montage shows one slider for them.
- **The "edit" flag:** a file and not an argument, because the save is asynchronous: the path only reaches the `-sc` hook once the file is written. The hook ignores a flag older than 10 s, so a save that failed doesn't make the next `/clip` open the editor.
- **No automatic copy with edit:** with `edit`, nothing goes to the clipboard until you export, which would replace it anyway.

On Windows and macOS, clips come from whichever recorder you use. NVIDIA app and OBS can write the mic to its own track, and Montage shows one row per track whatever the source.

## The stack

Everything Montage does is written in Rust; codecs come from FFmpeg's libraries, linked in, not run as a program. No pure-Rust decoder exists for HEVC, the recorder's codec, and none comes close for H.264 encoding, so linking libav is the only realistic way to keep the rest in Rust. libav also gives one API over each system's hardware decoders and encoders.

| Part | Choice | Why |
| --- | --- | --- |
| Window and UI | egui + egui-wgpu on a `winit` window | One windowing crate for Wayland, Windows and macOS. Génie's look, fonts and GPU code carry over; only its layer-shell surface doesn't, and Montage is an ordinary window anyway |
| Decode, encode, mux | libav through `ffmpeg-next` (or `rsmpeg`) | HEVC and H.264 decode on the GPU (see [Platforms](#platforms)), H.264 encode with x264 two-pass for size-capped exports on every system, and the GPU encoder for the fast path. nixpkgs has FFmpeg 9.0; the first spike checks which binding builds against it and against the Windows and Homebrew builds |
| Audio playback | `cpal` | PipeWire through ALSA, WASAPI, CoreAudio. The audio clock drives playback; video frames follow it |
| Text rendering | `cosmic-text` + `tiny-skia` | Already used in garde. cosmic-text loads every installed font through `fontdb`, which knows the font folders of all three systems; tiny-skia fills and strokes the glyph outlines, which gives the outline Impact captions need. The same code draws the preview and the export, so they match pixel for pixel |
| GIF | `gifski` | Best quality per byte by far (per-frame palettes, dithering across frames). It's AGPL-3.0, and so is Montage |
| Clipboard | per system, in `montage::platform::clipboard` | No cross-platform crate puts a *file* on the clipboard. See [Platforms](#platforms) |
| Save dialog | `rfd` | The desktop portal on Linux (COSMIC's file chooser), the native dialog on Windows and macOS |
| Trash | `trash` crate | The freedesktop trash, the Recycle Bin, the macOS Trash. Deleting can be undone everywhere |
| Notifications | `notify-rust` on Linux and macOS, `tauri-winrt-notification` on Windows | See phase 7 for the "Edit" action |
| Paths | `directories` | Videos folder, config folder and cache folder of each system |

## Platforms

The editor, the edit model, rendering, presets and the library are the same code everywhere. What differs is kept in `montage::platform`, one file per system, behind small functions:

| | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Hardware decode (preview) | NVDEC (`cuda`), VAAPI as fallback | D3D11VA (NVIDIA, AMD and Intel alike) | VideoToolbox |
| GPU encode (Full quality) | NVENC | NVENC, then AMF or QSV | VideoToolbox |
| Size-capped encode (Share) | x264 two-pass | x264 two-pass | x264 two-pass |
| Clipboard | `text/uri-list` through `wl-clipboard-rs`, served by a detached child that outlives the window, as `wl-copy` does today | `CF_HDROP` through `clipboard-win`. Windows keeps the data after Montage exits, so no child process | An `NSURL` written to `NSPasteboard` through `objc2-app-kit`. Also kept after exit |
| Low priority for `share` | nice 19 and ionice idle | `IDLE_PRIORITY_CLASS` and background mode (lower I/O and memory priority) | nice 19 and `setiopolicy_np` throttled I/O |
| Default library folders | `~/Videos/Replays`, `~/Videos/Recordings` | `Videos`, plus each recorder's folder added in the config | `~/Movies` |
| Config | `~/.config/montage/config.toml` | `%APPDATA%\montage\config.toml` | `~/Library/Application Support/montage/config.toml` |
| Impact | `corefonts` in nixos-config | Ships with Windows | Ships with macOS |
| Packaging | Nix flake | zip with the FFmpeg DLLs next to `montage.exe` | `.app` with the FFmpeg dylibs inside |

- **The preview always copies frames to the CPU** after hardware decode, then uploads them to a wgpu texture. Sharing GPU memory directly differs on every system and API; copying is portable and fast enough (a 1440p frame is 5.5 MB in NV12, so 144 fps is about 800 MB/s, which PCIe and Apple's unified memory both handle). Zero-copy can come later for one system without touching the others.
- **Every hardware path has a software fallback.** If the GPU decoder can't be opened, the preview decodes in software at a lower size; if no GPU encoder exists, Full quality uses x264. Share uses x264 everywhere, so a shared clip is the same size and quality on every system.
- **Linking FFmpeg:** nixpkgs on Linux, Homebrew for development on macOS, a shared "GPL" build (BtbN's) on Windows. Builds with x264 are GPL, which AGPL-3.0 can include.
- **Detached work** (serving the Wayland clipboard, waiting for a notification action) runs in a copy of Montage started with a hidden subcommand, not with `fork`, so it works the same way on all three.
- **File names** stay within what Windows accepts, and moving a file retries for a few seconds when Windows reports it still open, since a recorder may not have closed it yet.
- **CI** builds and tests on Linux, Windows and macOS for every push, so a change can't quietly break a system nobody is on.

## The editor

One window for one clip. From top to bottom:

1. **Preview** at the window's size, decoded on the GPU so 1440p at 144 fps plays at full rate. Space plays and pauses, `,` and `.` step one frame, J/K/L shuttle.
2. **Timeline** with a filmstrip of thumbnails, the waveform of the mixed audio underneath, and two handles for the start and end. `I` and `O` set them at the playhead. The part outside the cut is dimmed. A loop toggle replays just the cut.
3. **Audio:** one row per track ("Game", "Mic"; names in the config, "Track 1", "Track 2"… when unnamed), each with a mute toggle and a volume slider. There's a 50 ms fade at both cut points so the edges don't pop.
4. **Text:** "Add text" puts a caption on the preview, which you drag into place. Each caption has:
   - **A font:** any installed font (about 2,900 families on the Linux desktop), from a list you can search, with each name drawn in its own font. Impact is the default and pinned at the top, followed by recently used fonts.
   - **The classic caption look by default:** Impact, white, a thick black outline, in capitals, centred, at the top or bottom. "Capitals" is a toggle, so other styles stay possible.
   - **Size, fill colour, outline colour and width**, and an optional shadow.
   - **Timing:** the whole cut by default, with optional start and end handles on the timeline.

   Several captions are allowed. The edit saves the font by family name; if that font is later uninstalled, or the clip is opened on another computer that doesn't have it, the caption falls back to Impact and the editor says so.
5. **Export bar:** format (MP4 or GIF), destination (clipboard or file), a preset, the estimated size, and Export. Export runs in the background with a progress bar. When it's done you get the same notification as `share`, and the window can stay open for a second export.

Keyboard shortcuts use Cmd instead of Ctrl on macOS, through egui's `Modifiers::command`.

**Presets**

| Preset | What it does |
| --- | --- |
| Share (default MP4) | H.264 + AAC, at most 20 MiB, 1080p or 720p at 60 fps, with `gsr-clip-export`'s bitrate rules: at least 2,000 kbit/s of video (a longer clip is cut instead), 1080p from 4,500, at most 8,000, AAC at 128. All of it is set in `config.toml`'s `[share]` |
| Full quality | Copies the original video stream unchanged when nothing but the cut changed and the start falls on a keyframe (instant, lossless). Otherwise the GPU encoder at the original size and frame rate |
| GIF | 480 px wide, 20 fps, no audio, a size target (default 10 MB) that lowers the width or frame rate until it fits; tunable in the export bar |

Text forces a re-encode, so "Full quality" with text uses the GPU encoder.

**Edits are kept, not applied:** cut points, volumes and captions are saved with the clip (see below), and the original is never modified. Reopening the clip brings the edit back.

### One folder per clip

Each clip gets its own folder holding the original, its edit and its exports, inside whatever folder the recorder saved it in (on Linux, the day folders gpu-screen-recorder already makes):

```
~/Videos/Replays/2026-11-07/
  Replay_2026-11-07_21-03-11/
    Replay_2026-11-07_21-03-11.mp4         the original, never modified
    montage.toml                           the edit: cut, volumes, captions
    Replay_2026-11-07_21-03-11-share.mp4   the automatic copy (/clip)
    Replay_2026-11-07_21-03-11-edit.mp4    exports from the editor
    Replay_2026-11-07_21-03-11.gif
    .thumbs/                               filmstrip and waveform cache
```

- **Moving, deleting or starring a clip** handles the whole folder at once, so an edit can't get separated from its video.
- **Export names keep the clip's name.** Discord shows the name of the uploaded file, so `Replay_…-edit.mp4` stays meaningful once it leaves the folder; a second export of the same kind gets `-edit-2`, and so on.
- **Montage makes the folders.** gpu-screen-recorder can only write one file into the day folder. `montage share` and `montage open` start by moving that file into a folder of its own named after it, and the save hook runs one or the other for every save, so every new clip ends up in a folder.
- **Older clips, and clips from other recorders,** that are still loose files show up in the library like the others, and get their folder the first time they're opened or exported. Nothing is moved before that.
- **`montage.toml` holds no absolute paths,** so a clip folder can be copied to another computer, or another system, and opened there.
- **The thumbnail cache** lives in the clip's folder too, so it goes away with the clip instead of piling up in a cache folder. Windows hides it with the hidden attribute, since the leading dot means nothing there.

## The library

Opened from Montage's desktop entry, Start menu entry or app, or with Escape from the editor. It shows clips from the library folders (see [Platforms](#platforms); the list is in the config), newest first, grouped by day. On Linux the days are gpu-screen-recorder's day folders; elsewhere, the day each file was recorded. Each tile shows a thumbnail, the length and the size, plus a badge when the clip has an edit or an export. Thumbnails come from each clip's `.thumbs` folder, made the first time the clip is shown.

- **Star** a clip to mark it as kept.
- **Delete** sends the clip's whole folder to the trash, after a confirmation. You can select several to delete at once.
- A line at the top shows the total size of the folders (`648 MB` today on the Linux desktop, three days in), so it's visible when they're growing. Nothing is ever deleted unless you ask.

## Command line

```
montage                         the library
montage open <clip>             the editor
montage share <clip> [--type replay|regular|screenshot]
                                headless: the Share preset to the clipboard, with the notification;
                                what the recorder's save hook runs
montage export <clip> --preset share|full|gif [--out <path>] [--clipboard]
                                headless export using the clip's saved edit
montage settings                the settings page

Linux only (see recorder.md):
montage record start|stop|toggle
montage save [N] [edit]
montage on-save <file> <type>
montage recorder run
```

`<clip>` is the original file, loose or inside its folder, or the clip's folder.

`share` keeps today's behaviour: when the clip is too long for the budget, a replay keeps its end and a recording keeps its start. Without `--type`, a clip whose name starts with `Replay_` is a replay. It runs at low priority (nice 19 and idle I/O, or their Windows equivalents) as the script did, since the game is usually still running.

On Windows, `montage.exe` is built for the Windows subsystem, so opening it from Explorer doesn't flash a console; it attaches to the parent console when started from a terminal, so the commands above still print.

## Changes outside Montage

All Linux, all for the gpu-screen-recorder desktop:

- **nixos-config `modules/gpu-screen-recorder`:**
  - two `-a` tracks
  - `gsr-clip-export` removed; the save hook runs `montage share`, or `montage open` when the edit flag is present (both move the new file into its folder)
  - `gsr-replay` accepts `edit` and a number of seconds, in either order
  - the Génie entry's title mentions `/clip edit`
- **nixos-config `configuration.nix`:** `fonts.packages = [ pkgs.corefonts ]` for Impact, which isn't installed today. It's unfree, which the config already allows.
- **nixos-config `flake.nix`:** a `montage` input (`github:Sigma-studios/montage`, following `nixpkgs-apps`), with the package added to the home profile so the desktop entry shows up.
- **Génie:** nothing. `arguments = true` already passes `15 edit` through.
- **Notification action:** "Edit" on the "Clip copied" notification opens the clip in Montage.

## Phases

Each phase ends with something usable, on all three systems unless it says otherwise.

0. **Spike (go/no-go).** Decode a real 1440p144 HEVC replay on the GPU in Rust, upload the frames to a wgpu texture, play it with cpal audio in sync, and scrub. On Linux with NVDEC, on Windows with D3D11VA, on macOS with VideoToolbox. Also settle `ffmpeg-next` or `rsmpeg` against FFmpeg 9 from nixpkgs, BtbN's Windows build and Homebrew. If a GPU decoder turns out to be unusable on one system, its preview falls back to decoding at a lower size.
1. **Recorder controls, `montage share`, folders and two tracks** (revised by [`recorder.md`](recorder.md)). The CLI (`record`, `save`, `on-save`, `share`, `recorder run`), `config.toml`, moving each new clip into its own folder, the Share preset with `gsr-clip-export`'s rules, the clipboard on all three systems, CI on all three, the Home Manager module with the recorder service, and two audio tracks. The workspace: `montage-core` (folders, the edit model, the config, presets), `montage-media` (libav), `montage` (the binary and `platform`). Tests run on short clips generated at test time and check the size cap, the trim direction and the track mix-down.
2. **Editor: cut and audio, and the settings page.** Player, timeline, filmstrip, waveform, cut handles, per-track mute and volume, saving the edit to `montage.toml`, MP4 export to the clipboard or a file, and the Full quality fast path.
3. **`/clip edit`** (Linux). Now `save … edit`, part of phase 1, plus `montage open` from phase 2.
4. **Text.** Captions in the preview and composited into exports, the font picker, and Impact installed on the Linux desktop.
5. **GIF.** The GIF preset with its size target, to the clipboard or a file.
6. **Library.** Grid, thumbnail cache, stars, delete to trash, folder size.
7. **Notification action.** "Edit" on the share notification. On Linux, `share` sends it through `notify-rust` and waits for the action in its detached process. On Windows, the toast's button starts `montage open` through toast activation, which needs the Start menu shortcut to carry Montage's AppUserModelID. On macOS, notifications with buttons need a signed app bundle, so until Montage is signed the notification has no button and clicking it opens the clip.

Phases 4 to 7 don't depend on each other and can come in any order.

## Where it stands

2026-11-07: every phase is written, and tested on macOS (171 tests, plus the real window opened on a 1440p144 clip). Nothing has run on Linux or Windows yet; the Linux-specific code is type-checked from macOS, and CI builds and tests on all three systems.

| Phase | Written | Checked on macOS | Needs checking on the Linux desktop |
| --- | --- | --- | --- |
| 0. GPU decode, preview, audio sync | Player with NVDEC/VAAPI, D3D11VA, VideoToolbox, software fallback; audio clock through cpal | VideoToolbox: 144 fps played at about 97 % of frames | NVDEC and VAAPI, sync by ear |
| 1. Recorder controls, share, config, folders, Home Manager | All of it | With fake `gsr-cli`, `pw-play`, `systemd-run`; a real Share export | The real recorder, Wayland clipboard, systemd units, `nix build` |
| 2. Editor and settings page | Preview, timeline, cut, audio rows, export bar, settings with validation and service restart | Snapshot tests, a real clip opened, played, cut and exported | `gpu-screen-recorder --list-*` output, Wayland output names, `systemctl` restart |
| 3. `/clip edit` | `save … edit` and `montage open` | Fake programs | End to end from Génie |
| 4. Captions | Stroked outlines, font picker drawn in each font, recent fonts | Rendering tests, burned into all three presets | With `corefonts`' Impact and ~2,900 families |
| 5. GIF | gifski with the size target | Tests | — |
| 6. Library | Grid, thumbnails, stars, multi-select delete to the trash, total size | Snapshot tests, real thumbnails | The real trash |
| 7. "Edit" on the notification | Linux (D-Bus action), Windows (toast button), macOS (click) | Type-checked only | The notification server's action support |

## Decisions taken

- **License:** AGPL-3.0, which allows gifski.
- **Storage:** one folder per clip, holding the original, the edit and the exports.
- **Fonts:** every installed font can be used; Impact is the default, installed with `corefonts` on Linux and already present on Windows and macOS.
- **Systems:** Linux, Windows and macOS, from one codebase. Share uses x264 on all three so its output is the same everywhere; GPU decoders and encoders are picked per system with a software fallback.

## Open questions

- **libx264 in nixpkgs' `ffmpeg`.** The Nix package links the default `ffmpeg`; if it lacks libx264, switch to `ffmpeg-full`. CI's Nix job builds the package but doesn't run an export.
