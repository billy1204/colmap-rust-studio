//! Background tray process (`colmap-studio --tray`) and the window's client.
//!
//! Wayland cannot hide a window or reopen one inside the same process, so the
//! tray is a separate process that owns running jobs. Coordination lives in
//! $XDG_RUNTIME_DIR:
//! - `colmap-studio-tray.lock` + `.sock`: single tray process; one JSON
//!   request line per connection, one JSON response line back.
//! - `colmap-studio-gui.lock` + `.sock`: single window; accepts `show` and `quit`.

use super::{JobResult, run_job};
use colmap_launcher::{
    settings,
    studio_support::{PipelineEvent, PipelineKind},
};
use eframe::egui;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

const MAX_LOG_LINES: usize = 5000;

/// Exit code of a `--tray` launch that found another tray already running.
pub const EXIT_ALREADY_RUNNING: i32 = 3;

/// $XDG_RUNTIME_DIR, else a private per-user folder in /tmp (so two users
/// never share lock or socket files).
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("colmap-studio-{uid}"));
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    dir
}
fn tray_lock() -> PathBuf {
    runtime_dir().join("colmap-studio-tray.lock")
}
fn tray_sock() -> PathBuf {
    runtime_dir().join("colmap-studio-tray.sock")
}
fn gui_lock() -> PathBuf {
    runtime_dir().join("colmap-studio-gui.lock")
}
fn gui_sock() -> PathBuf {
    runtime_dir().join("colmap-studio-gui.sock")
}

/// Exclusive non-blocking flock; Some(file) while we hold it.
fn try_lock(p: &Path) -> Option<File> {
    // No truncation: the file is only a lock handle.
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(p)
        .ok()?;
    // SAFETY: plain flock on a descriptor we own.
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

/// Whether a tray process is alive, judged by its socket accepting
/// connections (not by the socket file existing, which may be stale).
fn tray_reachable() -> bool {
    UnixStream::connect(tray_sock()).is_ok()
}

/// Wait for a just-spawned `--tray` (or an already-running one it found) to
/// accept connections. Exit code 3 means another tray holds the lock and is
/// still starting, so keep waiting for its socket until the deadline.
fn wait_for_tray(mut child: std::process::Child, deadline: Instant) -> Result<(), String> {
    let mut exited = None;
    loop {
        if tray_reachable() {
            reap(child);
            return Ok(());
        }
        if exited.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                match status.code() {
                    Some(EXIT_ALREADY_RUNNING) => exited = Some(status),
                    Some(2) => return Err("no system tray host".into()),
                    _ => return Err(format!("tray process exited ({status})")),
                }
            }
        }
        if Instant::now() > deadline {
            reap(child);
            return Err("tray process did not start".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The program to re-launch: the AppImage itself when packaged (its mount
/// disappears with the launching process), else this executable.
fn self_exe() -> PathBuf {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("colmap-studio"))
}

/// Start a detached copy of ourselves in its own session.
fn spawn_detached(args: &[&str]) -> std::io::Result<std::process::Child> {
    let mut command = Command::new(self_exe());
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()
}

// ---------------------------------------------------------------- protocol

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Start {
        pipeline: PipelineKind,
        exe: PathBuf,
        images: PathBuf,
        workspace: PathBuf,
    },
    /// `job == 0` means whichever job is current.
    Status {
        job: u64,
        log_from: usize,
        stage_from: usize,
    },
    Cancel {
        job: u64,
    },
    /// Window → tray: Output parent folder and pending project folder
    /// (`parent` / project name). `pending` is None when the name is invalid.
    SetOutput {
        parent: PathBuf,
        pending: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize)]
enum Response {
    Started(u64),
    Status(Snapshot),
    Ok,
    Error(String),
}

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub job: u64,
    pub pipeline: PipelineKind,
    pub workspace: PathBuf,
    logs: Vec<String>,
    log_next: usize,
    stages: Vec<PipelineEvent>,
    stage_next: usize,
    result: Option<Result<JobResult, String>>,
}

/// Request from a background thread; may wait for a busy tray.
fn request(req: &Request) -> Result<Response, String> {
    request_with(req, Duration::from_secs(10))
}

fn request_with(req: &Request, timeout: Duration) -> Result<Response, String> {
    let mut stream = UnixStream::connect(tray_sock()).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&reply).map_err(|e| format!("Bad tray reply: {e}"))
}

// ------------------------------------------------------------ window side

/// Start the tray process in the background if it isn't already running, so
/// the tray icon is there whenever Studio is open. Failures are ignored; a
/// job hand-off will retry and report a missing tray host.
pub fn ensure_started() {
    if !tray_reachable() {
        let _ = spawn_detached(&["--tray"]).map(reap);
    }
}

/// Hand a job to the tray process, starting it if needed. Errors mean the
/// caller should run the job in the window (e.g. no tray host).
pub fn hand_off(
    pipeline: PipelineKind,
    exe: &Path,
    images: &Path,
    workspace: &Path,
) -> Result<u64, String> {
    if !tray_reachable() {
        let child = spawn_detached(&["--tray"]).map_err(|e| e.to_string())?;
        wait_for_tray(child, Instant::now() + Duration::from_secs(5))?;
    }
    // The tray may run with a different working directory and PATH (e.g. from
    // autostart), so it always gets an absolute COLMAP path.
    let exe = exe
        .canonicalize()
        .or_else(|_| std::path::absolute(exe))
        .map_err(|e| format!("COLMAP path {}: {e}", exe.display()))?;
    let start = Request::Start {
        pipeline,
        exe,
        images: images.to_owned(),
        workspace: workspace.to_owned(),
    };
    match request(&start)? {
        Response::Started(id) => Ok(id),
        Response::Error(e) => Err(e),
        _ => Err("unexpected tray reply".into()),
    }
}

/// A job the tray is currently running, if any (for a newly opened window).
pub fn running_job() -> Option<Snapshot> {
    let req = Request::Status {
        job: 0,
        log_from: usize::MAX,
        stage_from: usize::MAX,
    };
    // Called on the UI thread at startup: short timeouts, one retry.
    let reply = request_with(&req, Duration::from_millis(200))
        .or_else(|_| request_with(&req, Duration::from_millis(200)))
        .ok()?;
    match reply {
        Response::Status(s) if s.result.is_none() => Some(s),
        _ => None,
    }
}

/// Tell the tray the window's Output parent and pending project folder.
/// Called every frame from the UI thread, so it never blocks: changes are
/// handed to a background thread that talks to the tray, retrying once a
/// second while the tray is unreachable.
pub fn sync_output(parent: &Path, pending: Option<&Path>) {
    type Key = (PathBuf, Option<PathBuf>);
    static QUEUED: Mutex<Option<Key>> = Mutex::new(None);
    static WAKE: std::sync::Condvar = std::sync::Condvar::new();
    static LAST_SEEN: Mutex<Option<Key>> = Mutex::new(None);
    static WORKER: std::sync::Once = std::sync::Once::new();
    let key = (parent.to_owned(), pending.map(Path::to_owned));
    {
        let mut last = LAST_SEEN.lock().unwrap();
        if last.as_ref() == Some(&key) {
            return;
        }
        *last = Some(key.clone());
    }
    *QUEUED.lock().unwrap() = Some(key);
    WAKE.notify_one();
    WORKER.call_once(|| {
        std::thread::spawn(|| {
            let mut synced: Option<Key> = None;
            loop {
                let key = {
                    let mut queued = QUEUED.lock().unwrap();
                    loop {
                        if let Some(key) = queued.take() {
                            break key;
                        }
                        queued = WAKE.wait(queued).unwrap();
                    }
                };
                if synced.as_ref() == Some(&key) {
                    continue;
                }
                let req = Request::SetOutput {
                    parent: key.0.clone(),
                    pending: key.1.clone(),
                };
                if tray_reachable() && request_with(&req, Duration::from_secs(2)).is_ok() {
                    synced = Some(key);
                } else {
                    // Retry later unless a newer value arrived meanwhile.
                    std::thread::sleep(Duration::from_secs(1));
                    QUEUED.lock().unwrap().get_or_insert(key);
                }
            }
        });
    });
}

/// Runs in the window's worker thread: mirrors a tray job's logs and stage
/// events into the window's channels and returns its result. `detach` makes
/// it return immediately, leaving the job running.
pub fn proxy(
    job: u64,
    cancel: Arc<AtomicBool>,
    detach: Arc<AtomicBool>,
    tx: mpsc::SyncSender<String>,
    stage_tx: mpsc::Sender<PipelineEvent>,
) -> Result<JobResult, String> {
    let (mut log_from, mut stage_from) = (0, 0);
    let mut cancel_sent = false;
    loop {
        if detach.load(Ordering::SeqCst) {
            return Ok(JobResult::Cancelled);
        }
        if cancel.load(Ordering::SeqCst) && !cancel_sent {
            cancel_sent = true;
            let _ = request(&Request::Cancel { job });
        }
        let status = request(&Request::Status {
            job,
            log_from,
            stage_from,
        })
        .map_err(|e| format!("Lost the tray process: {e}"))?;
        match status {
            Response::Status(s) => {
                for line in s.logs {
                    let _ = tx.try_send(line);
                }
                for event in s.stages {
                    let _ = stage_tx.send(event);
                }
                log_from = s.log_next;
                stage_from = s.stage_next;
                if let Some(result) = s.result {
                    return result;
                }
            }
            Response::Error(e) => return Err(e),
            _ => return Err("unexpected tray reply".into()),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Single window instance. None = another window exists and was told to show.
pub enum GuiInstance {
    Primary(File, UnixListener),
    Shown,
    Unmanaged,
}

pub fn gui_instance() -> GuiInstance {
    match try_lock(&gui_lock()) {
        Some(lock) => {
            let _ = std::fs::remove_file(gui_sock());
            match UnixListener::bind(gui_sock()) {
                Ok(listener) => GuiInstance::Primary(lock, listener),
                Err(_) => GuiInstance::Unmanaged,
            }
        }
        None if send_gui("show") => GuiInstance::Shown,
        None => GuiInstance::Unmanaged,
    }
}

fn send_gui(cmd: &str) -> bool {
    UnixStream::connect(gui_sock())
        .and_then(|mut s| s.write_all(cmd.as_bytes()))
        .is_ok()
}

/// Bring the window forward when another launch (or the tray) asks.
pub fn listen_gui(listener: UnixListener, ctx: egui::Context) {
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut cmd = String::new();
            let _ = stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .and_then(|_| stream.read_to_string(&mut cmd));
            if cmd.trim() == "show" {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            } else if cmd.trim() == "quit" {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
        }
    });
}

// -------------------------------------------------------------- tray side

struct Job {
    id: u64,
    pipeline: PipelineKind,
    workspace: PathBuf,
    cancel: Arc<AtomicBool>,
    logs: VecDeque<String>,
    log_base: usize,
    stages: Vec<PipelineEvent>,
    stage: String,
    pgid: Option<i32>,
    paused: bool,
    result: Option<Result<JobResult, String>>,
}

impl Job {
    fn running(&self) -> bool {
        self.result.is_none()
    }
    /// Mark the job cancelled. Returns the process group to resume with
    /// SIGCONT (a stopped COLMAP can't see SIGTERM); send it with
    /// `resume_group` after releasing the state lock.
    fn cancel(&mut self) -> Option<i32> {
        self.cancel.store(true, Ordering::SeqCst);
        let paused = std::mem::take(&mut self.paused);
        self.pgid.filter(|_| paused)
    }
}

fn resume_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        // SAFETY: signals a COLMAP process group the tray's job created.
        unsafe { libc::killpg(pgid, libc::SIGCONT) };
    }
}

/// Stop a COLMAP process group for good: SIGCONT (in case it is paused),
/// SIGTERM, a short grace period, then SIGKILL.
fn stop_group(pgid: i32) {
    // SAFETY: plain signals to a process group this tray created.
    unsafe {
        libc::killpg(pgid, libc::SIGCONT);
        if libc::killpg(pgid, libc::SIGTERM) != 0 {
            return;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks that the group still exists.
        if unsafe { libc::killpg(pgid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: as above.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

/// Exit the tray without leaving COLMAP orphaned or frozen: cancel the job,
/// stop its process group, close the socket and exit.
fn shutdown(state: &State, code: i32) -> ! {
    // try_lock: never hang on exit if another thread wedged with the lock.
    let pgid = match state.try_lock() {
        Ok(mut shared) => shared.job.as_mut().filter(|j| j.running()).and_then(|j| {
            j.cancel();
            j.pgid
        }),
        Err(_) => Some(LAST_PGID.load(Ordering::SeqCst)).filter(|p| *p > 0),
    };
    if let Some(pgid) = pgid {
        stop_group(pgid);
    }
    let _ = std::fs::remove_file(tray_sock());
    std::process::exit(code);
}

/// Latest COLMAP process group (0 = none), readable without the state lock.
static LAST_PGID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SIGNAL_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_exit_signal(_: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write is async-signal-safe; the byte is just a wake-up.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// SIGTERM/SIGHUP/SIGINT (logout, `kill`, Ctrl-C) stop the job's COLMAP
/// processes before the tray exits. A self-pipe keeps the handler trivial.
fn handle_exit_signals(state: State) {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills two descriptors we then own.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    SIGNAL_PIPE.store(fds[1], Ordering::SeqCst);
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: installs a handler that only calls write(2).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_exit_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        // SAFETY: reads from the pipe's read end, owned by this thread.
        while unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), 1) } < 0 {}
        shutdown(&state, 0);
    });
}

#[derive(Default)]
struct Shared {
    job: Option<Job>,
    next_id: u64,
    notify: bool,
    /// Window's Output parent folder (also persisted in tray.json).
    output_parent: Option<PathBuf>,
    /// Pending project folder: Output parent + project name (from the window).
    pending_project: Option<PathBuf>,
    /// Workspace of the last finished job (survives after the run ends).
    last_workspace: Option<PathBuf>,
}

type State = Arc<Mutex<Shared>>;

static HANDLE: OnceLock<ksni::blocking::Handle<StudioTray>> = OnceLock::new();

/// Re-read status/icon/menu. Never call while holding the state lock.
fn refresh() {
    if let Some(h) = HANDLE.get() {
        h.update(|_| {});
    }
}

fn prefs_path() -> PathBuf {
    settings::default_path().with_file_name("tray.json")
}

#[derive(Serialize, Deserialize, Default)]
struct Prefs {
    notify: bool,
    #[serde(default)]
    output_parent: Option<PathBuf>,
}

fn save_prefs(prefs: &Prefs) {
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(prefs) {
        let _ = std::fs::write(path, text);
    }
}

fn autostart_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| colmap_launcher::platform::home_dir().join(".config"))
        .join("autostart")
        .join("colmap-studio-tray.desktop")
}

fn autostart_enabled() -> bool {
    autostart_path().is_file()
}

/// Quote an argument for a Desktop Entry `Exec` key: inside double quotes,
/// `"`, `` ` ``, `$` and `\` are backslash-escaped, then the whole value gets
/// the string-level escaping (`\` doubled) and `%` becomes `%%`.
fn desktop_exec_arg(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

/// Desktop Entry string value: escape backslash and control characters.
fn desktop_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

fn autostart_entry(exe: &Path) -> String {
    let exe = exe.display().to_string();
    format!(
        "[Desktop Entry]\nType=Application\nName=COLMAP Studio (tray)\n\
         Comment=Background reconstruction jobs\nTryExec={}\nExec={} --tray\n\
         Icon=colmap-studio\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        desktop_string(&exe),
        desktop_exec_arg(&exe),
    )
}

fn set_autostart(on: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !on {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, autostart_entry(&self_exe()))
}

/// With "Start at login" on, keep the entry pointing at the current program
/// (e.g. after the AppImage was moved or updated).
fn refresh_autostart() {
    if !autostart_enabled() {
        return;
    }
    let wanted = autostart_entry(&self_exe());
    if std::fs::read_to_string(autostart_path()).ok().as_deref() != Some(wanted.as_str()) {
        if let Err(e) = set_autostart(true) {
            eprintln!("colmap-studio: refresh start at login: {e}");
        }
    }
}

/// Ask a yes/no question in a small window (`colmap-studio --confirm`).
fn confirm(question: &str) -> bool {
    Command::new(self_exe())
        .args(["--confirm", question])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn open_studio() {
    if !send_gui("show") {
        let _ = spawn_detached(&[]).map(|mut child| {
            // Reap it in the background so it never lingers as a zombie.
            std::thread::spawn(move || child.wait());
        });
    }
}

fn notify_done(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "COLMAP Studio", "-i", "colmap-studio", summary, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut c| std::thread::spawn(move || c.wait()));
}

fn short(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() > max {
        format!("{}…", first.chars().take(max).collect::<String>())
    } else {
        first.to_owned()
    }
}

fn short_path(path: &Path, max: usize) -> String {
    let text = path.display().to_string();
    if text.chars().count() <= max {
        return text;
    }
    let keep = max.saturating_sub(1);
    let tail: String = text.chars().rev().take(keep).collect::<String>().chars().rev().collect();
    format!("…{tail}")
}

/// Open-folder target, in priority order:
/// running job workspace → pending project → output parent → last finished workspace.
fn open_target(shared: &Shared) -> Option<PathBuf> {
    let running = shared
        .job
        .as_ref()
        .filter(|j| j.running())
        .map(|j| &j.workspace);
    let finished = shared
        .last_workspace
        .as_ref()
        .or_else(|| shared.job.as_ref().map(|j| &j.workspace));
    [running, shared.pending_project.as_ref(), shared.output_parent.as_ref(), finished]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .cloned()
}

fn open_folder(path: &Path) {
    if let Err(e) = colmap_launcher::platform::open_in_shell(path) {
        let msg = format!("Couldn't open {}: {e}", path.display());
        eprintln!("colmap-studio: {msg}");
        notify_done(&msg, "");
    }
}

fn start_job(state: &State, pipeline: PipelineKind, exe: PathBuf, images: PathBuf, workspace: PathBuf) -> Response {
    let mut shared = state.lock().unwrap();
    if shared.job.as_ref().is_some_and(Job::running) {
        return Response::Error("A reconstruction is already running in the tray".into());
    }
    shared.next_id += 1;
    let id = shared.next_id;
    let cancel = Arc::new(AtomicBool::new(false));
    shared.job = Some(Job {
        id,
        pipeline,
        workspace: workspace.clone(),
        cancel: cancel.clone(),
        logs: VecDeque::new(),
        log_base: 0,
        stages: Vec::new(),
        stage: "preparing".into(),
        pgid: None,
        paused: false,
        result: None,
    });
    drop(shared);
    let (tx, rx) = mpsc::sync_channel::<String>(512);
    let (stage_tx, stage_rx) = mpsc::channel::<PipelineEvent>();
    let logs_state = state.clone();
    let logs = std::thread::spawn(move || {
        for line in rx {
            let mut stop = None;
            let mut shared = logs_state.lock().unwrap();
            let Some(job) = shared.job.as_mut().filter(|j| j.id == id) else { continue };
            if let Some(pid) = line
                .strip_prefix("Started COLMAP process ")
                .and_then(|p| p.trim().parse::<i32>().ok())
            {
                // job::execute makes each COLMAP process a group leader.
                job.pgid = Some(pid);
                LAST_PGID.store(pid, Ordering::SeqCst);
                if job.paused {
                    stop = Some(pid);
                }
            }
            if job.logs.len() >= MAX_LOG_LINES {
                job.logs.pop_front();
                job.log_base += 1;
            }
            job.logs.push_back(line);
            drop(shared);
            if let Some(pgid) = stop {
                // SAFETY: pauses the COLMAP group this job just started.
                unsafe { libc::killpg(pgid, libc::SIGSTOP) };
            }
        }
    });
    let stage_state = state.clone();
    let stages = std::thread::spawn(move || {
        for event in stage_rx {
            if let Some(job) = stage_state.lock().unwrap().job.as_mut().filter(|j| j.id == id) {
                if let PipelineEvent::Started(label) = &event {
                    job.stage = label.clone();
                }
                job.stages.push(event);
            }
            refresh();
        }
    });
    let worker_state = state.clone();
    std::thread::spawn(move || {
        refresh();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_job(pipeline, exe, images, workspace, cancel, tx, stage_tx)
        }))
        .unwrap_or_else(|_| Err("Background worker panicked".into()));
        let _ = logs.join();
        let _ = stages.join();
        let (summary, body) = match &result {
            Ok(JobResult::Cancelled) => ("Reconstruction cancelled", "Partial files were kept".to_owned()),
            Ok(_) => ("Reconstruction complete", "Open COLMAP Studio to view the result".to_owned()),
            Err(e) => ("Reconstruction failed", short(e, 200)),
        };
        let notify = {
            let mut shared = worker_state.lock().unwrap();
            let workspace = shared
                .job
                .as_ref()
                .filter(|j| j.id == id)
                .map(|j| j.workspace.clone());
            if let Some(job) = shared.job.as_mut().filter(|j| j.id == id) {
                job.pgid = None;
                LAST_PGID.store(0, Ordering::SeqCst);
                job.paused = false;
                job.result = Some(result);
            }
            if let Some(workspace) = workspace {
                shared.last_workspace = Some(workspace);
            }
            shared.notify
        };
        if notify {
            notify_done(summary, &body);
        }
        refresh();
    });
    Response::Started(id)
}

fn handle(state: &State, req: Request) -> Response {
    match req {
        Request::Start { exe, .. } if !exe.is_absolute() => Response::Error(format!(
            "COLMAP path must be absolute: {}",
            exe.display()
        )),
        Request::Start {
            pipeline,
            exe,
            images,
            workspace,
        } => start_job(state, pipeline, exe, images, workspace),
        Request::Status {
            job,
            log_from,
            stage_from,
        } => {
            let shared = state.lock().unwrap();
            let Some(j) = shared.job.as_ref().filter(|j| job == 0 || j.id == job) else {
                return Response::Error("That job is no longer tracked by the tray".into());
            };
            let log_next = j.log_base + j.logs.len();
            let skip = log_from.max(j.log_base).min(log_next) - j.log_base;
            Response::Status(Snapshot {
                job: j.id,
                pipeline: j.pipeline,
                workspace: j.workspace.clone(),
                logs: j.logs.iter().skip(skip).cloned().collect(),
                log_next,
                stages: j.stages.iter().skip(stage_from).cloned().collect(),
                stage_next: j.stages.len(),
                result: j.result.as_ref().map(|r| match r {
                    // JobResult isn't Clone; round-trip through JSON.
                    Ok(v) => serde_json::to_value(v)
                        .and_then(serde_json::from_value)
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.clone()),
                }),
            })
        }
        Request::Cancel { job } => {
            let resume = state
                .lock()
                .unwrap()
                .job
                .as_mut()
                .filter(|j| j.id == job)
                .and_then(Job::cancel);
            resume_group(resume);
            refresh();
            Response::Ok
        }
        Request::SetOutput { parent, pending } => {
            let mut shared = state.lock().unwrap();
            shared.output_parent = Some(parent);
            shared.pending_project = pending;
            save_prefs(&Prefs {
                notify: shared.notify,
                output_parent: shared.output_parent.clone(),
            });
            drop(shared);
            refresh();
            Response::Ok
        }
    }
}

struct StudioTray {
    state: State,
}

enum Look {
    Idle,
    Running,
    Paused,
    Failed,
}

impl StudioTray {
    fn look(&self) -> (Look, String) {
        let shared = self.state.lock().unwrap();
        let look = match &shared.job {
            Some(j) if j.running() && j.paused => Look::Paused,
            Some(j) if j.running() => Look::Running,
            Some(Job {
                result: Some(Err(_)),
                ..
            }) => Look::Failed,
            _ => Look::Idle,
        };
        (look, self_status(&shared))
    }
}

fn app_icon() -> Vec<Icon> {
    let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png"))
    else {
        return Vec::new();
    };
    // RGBA -> ARGB (network byte order) as StatusNotifierItem expects.
    let data = icon
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect();
    vec![Icon {
        width: icon.width as i32,
        height: icon.height as i32,
        data,
    }]
}

fn label(text: String) -> MenuItem<StudioTray> {
    StandardItem {
        label: text,
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for StudioTray {
    fn id(&self) -> String {
        "colmap-rust-studio".into()
    }
    fn title(&self) -> String {
        "COLMAP Studio".into()
    }
    fn icon_name(&self) -> String {
        match self.look().0 {
            Look::Idle => String::new(),
            Look::Running => "media-playback-start".into(),
            Look::Paused => "media-playback-pause".into(),
            Look::Failed => "dialog-error".into(),
        }
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        static ICON: OnceLock<Vec<Icon>> = OnceLock::new();
        match self.look().0 {
            Look::Idle => ICON.get_or_init(app_icon).clone(),
            _ => Vec::new(),
        }
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "COLMAP Studio".into(),
            description: self.look().1,
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        open_studio();
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let (status, running, paused, can_pause, target, notify) = {
            let shared = self.state.lock().unwrap();
            let job = shared.job.as_ref();
            (
                self_status(&shared),
                job.is_some_and(Job::running),
                job.is_some_and(|j| j.paused),
                job.is_some_and(|j| j.running() && j.pgid.is_some()),
                open_target(&shared),
                shared.notify,
            )
        };
        let mut items: Vec<MenuItem<Self>> = vec![
            label(status),
            MenuItem::Separator,
            StandardItem {
                label: "Open Studio".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|_| open_studio()),
                ..Default::default()
            }
            .into(),
        ];
        if running {
            items.push(
                CheckmarkItem {
                    label: "Pause".into(),
                    checked: paused,
                    enabled: can_pause,
                    activate: Box::new(|t: &mut Self| {
                        let toggled = t.state.lock().unwrap().job.as_mut().filter(|j| j.running()).map(|j| {
                            j.paused = !j.paused;
                            (j.pgid, j.paused)
                        });
                        if let Some((Some(pgid), paused)) = toggled {
                            let sig = if paused { libc::SIGSTOP } else { libc::SIGCONT };
                            // SAFETY: signals the job's COLMAP process group.
                            unsafe { libc::killpg(pgid, sig) };
                        }
                        refresh();
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Cancel job…".into(),
                    icon_name: "process-stop".into(),
                    activate: Box::new(|t: &mut Self| {
                        let state = t.state.clone();
                        std::thread::spawn(move || {
                            if confirm(
                                "Cancel the running reconstruction?\nPartial files will be kept.",
                            ) {
                                let resume = state.lock().unwrap().job.as_mut().and_then(Job::cancel);
                                resume_group(resume);
                                refresh();
                            }
                        });
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if let Some(target) = target {
            items.push(label(format!("Output: {}", short_path(&target, 48))));
            items.push(
                StandardItem {
                    label: "Open output folder".into(),
                    icon_name: "folder-open".into(),
                    activate: Box::new(|t: &mut Self| {
                        if let Some(path) = open_target(&t.state.lock().unwrap()) {
                            open_folder(&path);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
            MenuItem::Separator,
            CheckmarkItem {
                label: "Notify when done".into(),
                checked: notify,
                activate: Box::new(|t: &mut Self| {
                    let mut shared = t.state.lock().unwrap();
                    shared.notify = !shared.notify;
                    save_prefs(&Prefs {
                        notify: shared.notify,
                        output_parent: shared.output_parent.clone(),
                    });
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Start at login".into(),
                checked: autostart_enabled(),
                activate: Box::new(|_: &mut Self| {
                    if let Err(e) = set_autostart(!autostart_enabled()) {
                        eprintln!("colmap-studio: start at login: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let state = t.state.clone();
                    std::thread::spawn(move || quit(state));
                }),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("colmap-studio: tray host went away ({reason:?}); jobs keep running");
        true
    }
}

fn self_status(shared: &Shared) -> String {
    // Same wording as the tooltip, computed from an already-held lock.
    match &shared.job {
        None => "Idle".into(),
        Some(j) => match &j.result {
            None if j.cancel.load(Ordering::SeqCst) => "Cancelling…".into(),
            None if j.paused => format!("Paused — {}", short(&j.stage, 50)),
            None => format!("Running — {}", short(&j.stage, 50)),
            Some(Err(e)) => format!("Failed — {}", short(e, 50)),
            Some(Ok(JobResult::Cancelled)) => "Idle — last run cancelled".into(),
            Some(Ok(_)) => "Idle — last run complete".into(),
        },
    }
}

fn quit(state: State) {
    let running = state.lock().unwrap().job.as_ref().is_some_and(Job::running);
    if running {
        if !confirm("A reconstruction is running.\nQuit the tray and cancel it?") {
            return;
        }
        let resume = state.lock().unwrap().job.as_mut().and_then(Job::cancel);
        resume_group(resume);
        let deadline = Instant::now() + Duration::from_secs(30);
        while state.lock().unwrap().job.as_ref().is_some_and(Job::running) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // Quit from the tray closes the Studio window too.
    send_gui("quit");
    // Whatever is still running after the wait is stopped, not orphaned.
    shutdown(&state, 0);
}

/// `colmap-studio --tray`. Exit code 2 = no StatusNotifierItem host,
/// 3 = another tray is already running.
pub fn run() -> i32 {
    // A client probing the lock never takes it, but retry briefly anyway so
    // a racing launch doesn't misreport "already running".
    let mut lock = None;
    for _ in 0..5 {
        lock = try_lock(&tray_lock());
        if lock.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let Some(_lock) = lock else {
        eprintln!("colmap-studio: tray already running");
        return EXIT_ALREADY_RUNNING;
    };
    refresh_autostart();
    let prefs: Prefs = std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let state: State = Arc::new(Mutex::new(Shared {
        notify: prefs.notify,
        output_parent: prefs.output_parent,
        ..Default::default()
    }));
    handle_exit_signals(state.clone());
    use ksni::blocking::TrayMethods;
    match (StudioTray {
        state: state.clone(),
    })
    .spawn()
    {
        Ok(handle) => {
            let _ = HANDLE.set(handle);
        }
        Err(e) => {
            eprintln!(
                "colmap-studio: no system tray available ({e}). On GNOME, enable the \
                 AppIndicator extension."
            );
            return 2;
        }
    }
    // Bind only once the icon is registered, so a window never hands a job
    // to a tray process that is about to exit. We hold the lock, so any
    // existing socket file is stale (left by a crashed tray): remove it.
    let _ = std::fs::remove_file(tray_sock());
    let listener = match UnixListener::bind(tray_sock()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("colmap-studio: can't bind tray socket: {e}");
            return 1;
        }
    };
    for stream in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || {
            // A stalled client must not pin a handler thread forever.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            let reply = match serde_json::from_str::<Request>(&line) {
                Ok(req) => handle(&state, req),
                Err(e) => Response::Error(format!("bad request: {e}")),
            };
            if let Ok(text) = serde_json::to_string(&reply) {
                let _ = (&stream).write_all(text.as_bytes());
            }
        });
    }
    0
}

/// `colmap-studio --confirm "question"`: exit 0 for Yes, 1 for No.
pub fn confirm_window(question: String) -> i32 {
    struct Confirm {
        question: String,
        answer: Arc<AtomicBool>,
    }
    impl eframe::App for Confirm {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            ui.add_space(8.0);
            ui.label(&self.question);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Yes").clicked() {
                    self.answer.store(true, Ordering::SeqCst);
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("No").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        }
    }
    let answer = Arc::new(AtomicBool::new(false));
    let app = Confirm {
        question,
        answer: answer.clone(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([380.0, 130.0])
            .with_resizable(false)
            .with_app_id("colmap-rust-studio"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "COLMAP Studio",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(app))
        }),
    );
    if answer.load(Ordering::SeqCst) { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn autostart_entry_escapes_exec() {
        let entry = autostart_entry(Path::new("/home/u/My $Apps/100%/a\"b.AppImage"));
        assert!(entry.contains("Exec=\"/home/u/My \\\\$Apps/100%%/a\\\\\"b.AppImage\" --tray\n"), "{entry}");
        assert!(entry.contains("TryExec=/home/u/My $Apps/100%/a\"b.AppImage\n"), "{entry}");
        assert!(entry.contains("Icon=colmap-studio\n"));
    }
}
