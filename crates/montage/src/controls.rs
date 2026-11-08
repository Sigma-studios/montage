//! The recorder controls: `record`, `save`, `on-save` and `recorder run`.
//!
//! They replace the shell scripts that drove gpu-screen-recorder, with the
//! same behaviour: the recorder is told what to do through `gsr-cli`, sounds
//! play through `pw-play` under the `gsr-chime` application name so the
//! recording leaves them out, and nothing waits for a sound. Every command
//! but `recorder run` returns in well under 100 ms, since they're bound to
//! keys; none of them touches the GPU, fonts or libav.
//!
//! Linux only at run time; the logic compiles everywhere and is tested with
//! fake programs.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use montage_core::Clip;
use montage_core::Config;
use montage_core::recorder::{
    self, CHIME_APP, EDIT_FLAG, IPC_SOCKET, ON_SAVE_NAME, RECORDING_FLAG, Sound,
};

/// The programs the controls run. Each comes from an environment variable,
/// then from the path baked in at build time (the Nix package sets them),
/// then from `PATH`.
#[derive(Debug, Clone)]
pub struct Programs {
    pub gsr_cli: OsString,
    pub pw_play: OsString,
    pub systemd_run: OsString,
    pub recorder: OsString,
}

macro_rules! program {
    ($var:literal, $default:literal) => {
        env::var_os($var)
            .or(option_env!($var).map(OsString::from))
            .unwrap_or_else(|| OsString::from($default))
    };
}

impl Programs {
    pub fn from_env() -> Programs {
        Programs {
            gsr_cli: program!("MONTAGE_GSR_CLI", "gsr-cli"),
            pw_play: program!("MONTAGE_PW_PLAY", "pw-play"),
            systemd_run: program!("MONTAGE_SYSTEMD_RUN", "systemd-run"),
            recorder: program!("MONTAGE_GPU_SCREEN_RECORDER", "gpu-screen-recorder"),
        }
    }
}

/// Shows a desktop notification: summary, then body.
pub type Notifier = Box<dyn Fn(&str, &str)>;

pub struct Controls {
    pub config: Config,
    pub runtime_dir: PathBuf,
    pub data_dirs: Vec<PathBuf>,
    pub programs: Programs,
    /// Montage itself, for what `on-save` starts.
    pub self_exe: PathBuf,
    pub notify: Notifier,
}

impl Controls {
    pub fn from_env(config: Config, notify: Notifier) -> Result<Controls> {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR isn't set")?;
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let data_dirs = recorder::data_dirs(
            env::var_os("XDG_DATA_HOME"),
            env::var_os("XDG_DATA_DIRS"),
            &home,
        );
        Ok(Controls {
            config,
            runtime_dir,
            data_dirs,
            programs: Programs::from_env(),
            self_exe: env::current_exe().context("can't find Montage's own path")?,
            notify,
        })
    }

    fn recording_flag(&self) -> PathBuf {
        self.runtime_dir.join(RECORDING_FLAG)
    }

    fn edit_flag(&self) -> PathBuf {
        self.runtime_dir.join(EDIT_FLAG)
    }

    /// Runs `gsr-cli -ipc <socket> <args>`.
    fn gsr(&self, args: &[&str]) -> Result<()> {
        let status = Command::new(&self.programs.gsr_cli)
            .arg("-ipc")
            .arg(self.runtime_dir.join(IPC_SOCKET))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("can't run {}", self.programs.gsr_cli.display()))?;
        if !status.success() {
            bail!("gsr-cli {} failed ({status})", args.join(" "));
        }
        Ok(())
    }

    /// Starts a sound and returns at once.
    pub fn play(&self, sound: Sound) {
        let settings = self.config.sounds;
        if !settings.enabled {
            return;
        }
        let Some(file) = sound.find(&self.data_dirs) else {
            eprintln!("montage: no {}.oga in the sound theme", sound.name());
            return;
        };
        let started = Command::new(&self.programs.pw_play)
            .arg("--volume")
            .arg(settings.volume.to_string())
            .arg(&file)
            .env(
                "PIPEWIRE_PROPS",
                format!("{{ application.name = {CHIME_APP} }}"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = started {
            eprintln!("montage: can't play {}: {e}", file.display());
        }
    }

    /// `montage record start`.
    pub fn record_start(&self) -> Result<()> {
        if let Err(e) = self.gsr(&["start-replay-recording"]) {
            self.play(Sound::Error);
            return Err(e);
        }
        if let Err(e) = fs::write(self.recording_flag(), b"") {
            eprintln!(
                "montage: recording, but can't write {}: {e}",
                self.recording_flag().display()
            );
        }
        self.play(Sound::RecordingStarted);
        Ok(())
    }

    /// `montage record stop`. The "saved" sound comes from `on-save`, once
    /// the file is closed.
    pub fn record_stop(&self) -> Result<()> {
        self.gsr(&["stop-replay-recording"]).inspect_err(|_| {
            self.play(Sound::Error);
        })
    }

    /// `montage record toggle`: stops a running recording, or starts one.
    pub fn record_toggle(&self) -> Result<()> {
        if self.recording_flag().exists() {
            self.record_stop()
        } else {
            self.record_start()
        }
    }

    /// `montage save [N] [edit]`.
    pub fn save<S: AsRef<str>>(&self, args: &[S]) -> Result<()> {
        let request = match recorder::parse_save_args(args, self.config.recorder.replay_seconds) {
            Ok(request) => request,
            Err(arg) => {
                let message = format!("“{arg}” isn't a number of seconds.");
                (self.notify)("Clip not saved", &message);
                self.play(Sound::Error);
                bail!("{message}");
            }
        };
        let flag = self.edit_flag();
        if request.edit {
            fs::write(&flag, b"").with_context(|| format!("can't write {}", flag.display()))?;
        }
        if let Err(e) = self.gsr(&["save-replay", &request.seconds.to_string()]) {
            if request.edit {
                let _ = fs::remove_file(&flag);
            }
            self.play(Sound::Error);
            return Err(e);
        }
        Ok(())
    }

    /// `montage on-save <file> <type>`, the recorder's `-sc` hook.
    pub fn on_save(&self, file: &Path, kind: &str) -> Result<()> {
        match kind {
            "screenshot" => return Ok(()),
            "replay" => self.play(Sound::ReplaySaved),
            "regular" => {
                remove_if_present(&self.recording_flag())?;
                self.play(Sound::RecordingSaved);
            }
            other => bail!("unknown recording type {other:?}"),
        }

        let clip = Clip::locate(file)?.adopt()?;
        let mut args: Vec<OsString> = Vec::new();
        let edit = kind == "replay" && self.take_edit_flag();
        if edit {
            args.push("open".into());
            args.push(clip.original.into());
        } else {
            args.push("share".into());
            args.push(clip.original.into());
            args.push("--type".into());
            args.push(kind.into());
        }
        self.detach(&args, !edit)
    }

    /// Removes the edit flag and says whether it was there and fresh.
    fn take_edit_flag(&self) -> bool {
        let flag = self.edit_flag();
        let Ok(written) = fs::metadata(&flag).and_then(|m| m.modified()) else {
            return false;
        };
        let _ = fs::remove_file(&flag);
        recorder::edit_flag_is_fresh(written, SystemTime::now())
    }

    /// Starts Montage with `args` in a unit of its own, out of the
    /// recorder's cgroup, so it outlives a restart of the recorder: what
    /// serves the clipboard has to. `background` runs it at nice 19 and idle
    /// I/O.
    fn detach(&self, args: &[OsString], background: bool) -> Result<()> {
        let mut command = Command::new(&self.programs.systemd_run);
        command.args(["--user", "--collect", "--quiet"]);
        if background {
            command.args(["--nice=19", "--property=IOSchedulingClass=idle"]);
        }
        for var in ["WAYLAND_DISPLAY", "DISPLAY"] {
            if let Some(value) = env::var_os(var) {
                let mut setenv = OsString::from(format!("--setenv={var}="));
                setenv.push(value);
                command.arg(setenv);
            }
        }
        command.arg("--").arg(&self.self_exe).args(args);
        let status = command.stdin(Stdio::null()).stdout(Stdio::null()).status();
        match status {
            Ok(status) if status.success() => Ok(()),
            failed => {
                let why = match failed {
                    Ok(status) => status.to_string(),
                    Err(e) => e.to_string(),
                };
                eprintln!(
                    "montage: systemd-run failed ({why}); starting it directly, \
                     so a restart of the recorder will stop it"
                );
                self.spawn_directly(args)
            }
        }
    }

    fn spawn_directly(&self, args: &[OsString]) -> Result<()> {
        let mut command = Command::new(&self.self_exe);
        command.args(args).stdin(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
            .spawn()
            .with_context(|| format!("can't start {}", self.self_exe.display()))?;
        Ok(())
    }

    /// The gpu-screen-recorder command `montage recorder run` replaces itself
    /// with: the config's settings, Montage as the `-sc` hook, and the
    /// control socket in the runtime folder. Creates the folders it needs.
    pub fn recorder_command(&self) -> Result<Command> {
        let settings = &self.config.recorder;
        for dir in [&settings.replay_dir, &settings.record_dir] {
            fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
        }
        let hook = self.install_on_save_hook()?;
        let mut command = Command::new(&self.programs.recorder);
        command.args(settings.args(&hook, &self.runtime_dir.join(IPC_SOCKET)));
        Ok(command)
    }

    /// `$XDG_RUNTIME_DIR/montage/montage-on-save`, a script that runs
    /// `montage on-save` with the recorder's arguments, so the recorder's
    /// hook is a plain path. Not a link to Montage under that name:
    /// gpu-screen-recorder resolves the hook's links before running it, so
    /// Montage would start under its own name and take the file for a
    /// command.
    fn install_on_save_hook(&self) -> Result<PathBuf> {
        let dir = self.runtime_dir.join("montage");
        fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
        let link = dir.join(ON_SAVE_NAME);
        remove_if_present(&link)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = on_save_script(&self.self_exe);
            let partial = dir.join(format!(".{ON_SAVE_NAME}.partial"));
            fs::write(&partial, script)
                .and_then(|()| fs::set_permissions(&partial, fs::Permissions::from_mode(0o755)))
                .and_then(|()| fs::rename(&partial, &link))
                .with_context(|| format!("can't create {}", link.display()))?;
        }
        #[cfg(not(unix))]
        fs::copy(&self.self_exe, &link)
            .with_context(|| format!("can't create {}", link.display()))?;
        Ok(link)
    }

    /// `montage recorder run`: replaces Montage with gpu-screen-recorder, so
    /// the service supervises the recorder itself.
    pub fn recorder_run(&self) -> Result<()> {
        let mut command = self.recorder_command()?;
        #[cfg(unix)]
        {
            let error = std::os::unix::process::CommandExt::exec(&mut command);
            Err(error).with_context(|| format!("can't run {}", self.programs.recorder.display()))
        }
        #[cfg(not(unix))]
        {
            let _ = &mut command;
            bail!("gpu-screen-recorder only runs on Linux")
        }
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("can't remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The hook script: `montage on-save` with whatever the recorder passes.
#[cfg(unix)]
fn on_save_script(montage: &Path) -> String {
    let quoted = montage.to_string_lossy().replace('\'', "'\\''");
    format!("#!/bin/sh\nexec '{quoted}' on-save \"$@\"\n")
}

/// Whether Montage was started as the recorder's hook, under
/// `montage-on-save` (a link, as earlier versions installed).
pub fn started_as_on_save(argv0: &OsStr) -> bool {
    Path::new(argv0).file_name() == Some(OsStr::new(ON_SAVE_NAME))
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::*;

    /// A sandbox with fake programs that log their arguments, one line per
    /// call, to `<name>.log`.
    struct Sandbox {
        // Tests run one at a time: a fake program being written while
        // another test forks can't be executed ("text file busy").
        _serial: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        root: PathBuf,
        notes: Rc<RefCell<Vec<(String, String)>>>,
        controls: Controls,
    }

    fn fake(dir: &Path, name: &str, exit: i32) -> OsString {
        let path = dir.join(name);
        let log = dir.join(format!("{name}.log"));
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*|${{PIPEWIRE_PROPS}}\" >> '{}'\nexit {exit}\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.into()
    }

    impl Sandbox {
        fn new(gsr_exit: i32) -> Sandbox {
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let bin = root.join("bin");
            let runtime = root.join("run");
            let data = root.join("share");
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(&runtime).unwrap();
            let sounds = data.join("sounds/freedesktop/stereo");
            fs::create_dir_all(&sounds).unwrap();
            for sound in [
                Sound::RecordingStarted,
                Sound::RecordingSaved,
                Sound::ReplaySaved,
                Sound::Error,
            ] {
                fs::write(sounds.join(format!("{}.oga", sound.name())), b"").unwrap();
            }

            let mut config = Config::default();
            config.recorder.replay_dir = root.join("Videos/Replays");
            config.recorder.record_dir = root.join("Videos/Recordings");
            let notes = Rc::new(RefCell::new(Vec::new()));
            let sink = notes.clone();
            let controls = Controls {
                config,
                runtime_dir: runtime,
                data_dirs: vec![data],
                programs: Programs {
                    gsr_cli: fake(&bin, "gsr-cli", gsr_exit),
                    pw_play: fake(&bin, "pw-play", 0),
                    systemd_run: fake(&bin, "systemd-run", 0),
                    recorder: fake(&bin, "gpu-screen-recorder", 0),
                },
                self_exe: "/nix/store/montage/bin/montage".into(),
                notify: Box::new(move |summary, body| {
                    sink.borrow_mut().push((summary.into(), body.into()))
                }),
            };
            Sandbox {
                _serial: serial,
                _tmp: tmp,
                root,
                notes,
                controls,
            }
        }

        fn log(&self, program: &str) -> Vec<String> {
            fs::read_to_string(self.root.join("bin").join(format!("{program}.log")))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Sounds play without being waited for: poll until `n` played.
        fn sounds(&self, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let log = self.log("pw-play");
                if log.len() >= n || Instant::now() > deadline {
                    return log
                        .iter()
                        .map(|line| {
                            assert!(
                                line.ends_with("|{ application.name = gsr-chime }"),
                                "{line}"
                            );
                            let file = line.split('|').next().unwrap();
                            assert!(file.starts_with("--volume 0.6 "), "{line}");
                            Path::new(file)
                                .file_stem()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn gsr_calls(&self) -> Vec<String> {
            let socket = self.controls.runtime_dir.join("gsr.sock");
            self.log("gsr-cli")
                .iter()
                .map(|line| {
                    let args = line.split('|').next().unwrap();
                    let prefix = format!("-ipc {} ", socket.display());
                    args.strip_prefix(&prefix)
                        .unwrap_or_else(|| panic!("{line}"))
                        .to_string()
                })
                .collect()
        }

        fn recording_flag(&self) -> PathBuf {
            self.controls.runtime_dir.join("gsr-recording")
        }

        fn edit_flag(&self) -> PathBuf {
            self.controls.runtime_dir.join("montage-edit-next")
        }

        fn new_replay(&self) -> PathBuf {
            let day = self.root.join("Videos/Replays/2026-11-07");
            fs::create_dir_all(&day).unwrap();
            let file = day.join("Replay_2026-11-07_21-03-11.mp4");
            fs::write(&file, b"").unwrap();
            file
        }
    }

    #[test]
    fn record_start_marks_the_recording() {
        let s = Sandbox::new(0);
        s.controls.record_start().unwrap();
        assert_eq!(s.gsr_calls(), ["start-replay-recording"]);
        assert!(s.recording_flag().exists());
        assert_eq!(s.sounds(1), ["device-added"]);
    }

    #[test]
    fn record_start_failure_plays_the_error() {
        let s = Sandbox::new(1);
        assert!(s.controls.record_start().is_err());
        assert!(!s.recording_flag().exists());
        assert_eq!(s.sounds(1), ["dialog-error"]);
    }

    #[test]
    fn record_stop_is_silent_on_success() {
        let s = Sandbox::new(0);
        s.controls.record_stop().unwrap();
        assert_eq!(s.gsr_calls(), ["stop-replay-recording"]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(s.sounds(0).is_empty());
        drop(s);

        let s = Sandbox::new(1);
        assert!(s.controls.record_stop().is_err());
        assert_eq!(s.sounds(1), ["dialog-error"]);
    }

    #[test]
    fn record_toggle_follows_the_flag() {
        let s = Sandbox::new(0);
        s.controls.record_toggle().unwrap();
        fs::write(s.recording_flag(), b"").unwrap();
        s.controls.record_toggle().unwrap();
        assert_eq!(
            s.gsr_calls(),
            ["start-replay-recording", "stop-replay-recording"]
        );
    }

    #[test]
    fn save_passes_the_length() {
        let s = Sandbox::new(0);
        s.controls.save::<&str>(&[]).unwrap();
        s.controls.save(&["15"]).unwrap();
        s.controls.save(&["600"]).unwrap();
        assert_eq!(
            s.gsr_calls(),
            ["save-replay 60", "save-replay 15", "save-replay 60"]
        );
        assert!(!s.edit_flag().exists());
    }

    #[test]
    fn save_refuses_a_bad_length() {
        let s = Sandbox::new(0);
        assert!(s.controls.save(&["15s"]).is_err());
        assert!(s.gsr_calls().is_empty());
        assert_eq!(
            *s.notes.borrow(),
            [(
                "Clip not saved".to_string(),
                "“15s” isn't a number of seconds.".to_string()
            )]
        );
        assert_eq!(s.sounds(1), ["dialog-error"]);
    }

    #[test]
    fn save_edit_leaves_the_flag() {
        let s = Sandbox::new(0);
        s.controls.save(&["edit", "15"]).unwrap();
        assert_eq!(s.gsr_calls(), ["save-replay 15"]);
        assert!(s.edit_flag().exists());
    }

    #[test]
    fn failed_save_edit_removes_the_flag() {
        let s = Sandbox::new(1);
        assert!(s.controls.save(&["15", "edit"]).is_err());
        assert!(!s.edit_flag().exists());
        assert_eq!(s.sounds(1), ["dialog-error"]);
    }

    #[test]
    fn on_save_moves_the_replay_and_shares_it() {
        let s = Sandbox::new(0);
        let file = s.new_replay();
        s.controls.on_save(&file, "replay").unwrap();

        let original = file
            .parent()
            .unwrap()
            .join("Replay_2026-11-07_21-03-11/Replay_2026-11-07_21-03-11.mp4");
        assert!(original.exists());
        assert_eq!(s.sounds(1), ["complete"]);
        let run = s.log("systemd-run");
        assert_eq!(run.len(), 1);
        let args = run[0].split('|').next().unwrap();
        assert!(
            args.starts_with(
                "--user --collect --quiet --nice=19 --property=IOSchedulingClass=idle "
            ),
            "{args}"
        );
        assert!(
            args.ends_with(&format!(
                "-- /nix/store/montage/bin/montage share {} --type replay",
                original.display()
            )),
            "{args}"
        );
    }

    #[test]
    fn on_save_opens_the_editor_after_save_edit() {
        let s = Sandbox::new(0);
        s.controls.save(&["edit"]).unwrap();
        let file = s.new_replay();
        s.controls.on_save(&file, "replay").unwrap();
        assert!(!s.edit_flag().exists(), "the flag is consumed");
        let args = s.log("systemd-run").remove(0);
        let args = args.split('|').next().unwrap();
        assert!(!args.contains("--nice"), "{args}");
        assert!(
            args.contains("/nix/store/montage/bin/montage open "),
            "{args}"
        );
    }

    #[test]
    fn a_stale_edit_flag_is_dropped() {
        let s = Sandbox::new(0);
        let flag = s.edit_flag();
        fs::write(&flag, b"").unwrap();
        let old = SystemTime::now() - Duration::from_secs(30);
        fs::File::options()
            .write(true)
            .open(&flag)
            .unwrap()
            .set_modified(old)
            .unwrap();
        s.controls.on_save(&s.new_replay(), "replay").unwrap();
        assert!(!flag.exists());
        assert!(s.log("systemd-run")[0].contains(" share "));
    }

    #[test]
    fn on_save_of_a_recording_clears_the_red_dot() {
        let s = Sandbox::new(0);
        fs::write(s.recording_flag(), b"").unwrap();
        fs::write(s.edit_flag(), b"").unwrap();
        let day = s.root.join("Videos/Recordings/2026-11-07");
        fs::create_dir_all(&day).unwrap();
        let file = day.join("Video_2026-11-07_21-10-00.mp4");
        fs::write(&file, b"").unwrap();

        s.controls.on_save(&file, "regular").unwrap();
        assert!(!s.recording_flag().exists());
        assert!(
            s.edit_flag().exists(),
            "only a replay consumes the edit flag"
        );
        assert_eq!(s.sounds(1), ["device-removed"]);
        assert!(s.log("systemd-run")[0].contains(" share "));
        assert!(s.log("systemd-run")[0].contains("--type regular"));
    }

    #[test]
    fn on_save_ignores_screenshots() {
        let s = Sandbox::new(0);
        let file = s.root.join("shot.png");
        fs::write(&file, b"").unwrap();
        s.controls.on_save(&file, "screenshot").unwrap();
        assert!(file.exists());
        assert!(s.log("systemd-run").is_empty());
    }

    #[test]
    fn falls_back_when_systemd_run_fails() {
        let mut s = Sandbox::new(0);
        let bin = s.root.join("bin");
        s.controls.programs.systemd_run = fake(&bin, "systemd-run", 1);
        s.controls.self_exe = PathBuf::from(fake(&bin, "montage", 0));
        s.controls.on_save(&s.new_replay(), "replay").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.log("montage").is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.log("montage")[0].starts_with("share "));
    }

    #[test]
    fn sounds_can_be_turned_off() {
        let mut s = Sandbox::new(0);
        s.controls.config.sounds.enabled = false;
        s.controls.record_start().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(s.log("pw-play").is_empty());
    }

    #[test]
    fn the_hook_script_runs_on_save_even_through_a_resolved_path() {
        // What gpu-screen-recorder does: resolve the hook, then run it.
        let s = Sandbox::new(0);
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("args");
        let fake = tmp.path().join("it's montage");
        fs::write(&fake, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let hook = s.controls.runtime_dir.join("montage").join(ON_SAVE_NAME);
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, on_save_script(&fake)).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let status = Command::new(fs::canonicalize(&hook).unwrap())
            .args(["/videos/Replay 1.mp4", "replay"])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(&log).unwrap(), "on-save\n/videos/Replay 1.mp4\nreplay\n");
    }

    #[test]
    fn recorder_command_uses_the_config() {
        let s = Sandbox::new(0);
        let command = s.controls.recorder_command().unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let hook = s.controls.runtime_dir.join("montage/montage-on-save");
        let at = |flag: &str| {
            let i = args.iter().position(|a| a == flag).unwrap();
            args[i + 1].clone()
        };
        assert_eq!(at("-sc"), hook.display().to_string());
        assert_eq!(
            fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\nexec '/nix/store/montage/bin/montage' on-save \"$@\"\n"
        );
        assert_eq!(at("-a"), "app-inverse:gsr-chime");
        assert_eq!(
            at("-ipc"),
            s.controls
                .runtime_dir
                .join("gsr.sock")
                .display()
                .to_string()
        );
        assert!(s.root.join("Videos/Replays").is_dir());
        assert!(s.root.join("Videos/Recordings").is_dir());
        // Running it twice replaces the link.
        s.controls.recorder_command().unwrap();
    }
}
