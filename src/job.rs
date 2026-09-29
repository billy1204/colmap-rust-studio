use std::path::Path;
use std::process::Command;
use std::sync::{Arc, atomic::AtomicBool, mpsc::SyncSender};
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Completed,
    Cancelled,
}

#[derive(Debug)]
pub struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum CaptureOutcome {
    Completed(CapturedOutput),
    Cancelled,
}
pub fn execute(
    command: &mut Command,
    log: &Path,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
) -> Result<Outcome, String> {
    execute_with_mode(command, log, cancel, events, false)
}

/// Append another stage to an existing pipeline log. The first stage must use
/// `execute`, preserving create-new protection for the overall run.
pub fn execute_append(
    command: &mut Command,
    log: &Path,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
) -> Result<Outcome, String> {
    execute_with_mode(command, log, cancel, events, true)
}

/// Run a short helper process under the same owned Windows Job Object used by
/// reconstruction jobs, while retaining its output for parsing.
pub fn capture(
    command: &mut Command,
    cancel: Arc<AtomicBool>,
    timeout: std::time::Duration,
) -> Result<CaptureOutcome, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    if cancel.load(Ordering::SeqCst) {
        return Ok(CaptureOutcome::Cancelled);
    }
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut guard = OwnedChild::attach(child)?;
    let mut stdout = guard.child.stdout.take().ok_or("Missing stdout pipe")?;
    let mut stderr = guard.child.stderr.take().ok_or("Missing stderr pipe")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = guard.child.try_wait().map_err(|e| e.to_string())? {
            guard.close_job();
            break status;
        }
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
        } else if Instant::now() >= deadline {
            timed_out = true;
        }
        if cancelled || timed_out {
            guard.close_job();
            let _ = guard.child.kill();
            break guard.child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "Stdout reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "Stderr reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    if cancelled {
        return Ok(CaptureOutcome::Cancelled);
    }
    if timed_out {
        return Err(format!(
            "Helper process timed out after {}s",
            timeout.as_secs()
        ));
    }
    if !status.success() {
        return Err(format!(
            "Helper process exited with {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(CaptureOutcome::Completed(CapturedOutput { stdout, stderr }))
}

fn execute_with_mode(
    command: &mut Command,
    log: &Path,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
    append: bool,
) -> Result<Outcome, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::sync::{atomic::Ordering, mpsc};
    use std::time::Duration;
    if cancel.load(Ordering::SeqCst) {
        return Ok(Outcome::Cancelled);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if append {
        options.append(true);
        if !log.is_file() {
            return Err(format!("Pipeline log does not exist: {}", log.display()));
        }
    } else {
        options.create_new(true);
    }
    let mut file = options.open(log).map_err(|e| e.to_string())?;
    if append {
        writeln!(file, "\n--- NEXT PIPELINE STAGE ---").map_err(|e| e.to_string())?;
    }
    writeln!(file, "Command: {command:?}").map_err(|e| e.to_string())?;
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW: no extra terminal flashes.
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut guard = OwnedChild::attach(child)?;
    let _ = events.try_send(format!("Started COLMAP process {}", guard.child.id()));
    writeln!(file, "PID: {}", guard.child.id()).map_err(|e| e.to_string())?;
    let stdout = guard.child.stdout.take().ok_or("Missing stdout pipe")?;
    let stderr = guard.child.stderr.take().ok_or("Missing stderr pipe")?;
    let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(256);
    let tx2 = tx.clone();
    let a = std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            if tx.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let b = std::thread::spawn(move || {
        for line in BufReader::new(stderr).split(b'\n') {
            if tx2.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let mut cancelled = false;
    let mut status = None;
    let mut disconnected = false;
    let mut failure = None;
    while status.is_none() || !disconnected {
        if cancel.load(Ordering::SeqCst) && status.is_none() && !cancelled {
            cancelled = true;
            guard.close_job();
            if let Err(e) = guard.child.kill()
                && guard.child.try_wait().ok().flatten().is_none()
            {
                failure = Some(format!("Could not stop child: {e}"));
            }
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(line)) => {
                if let Err(e) = file.write_all(&line).and_then(|_| file.write_all(b"\n")) {
                    failure = Some(format!("Log write failed: {e}"));
                    guard.close_job();
                    let _ = guard.child.kill();
                }
                let _ = events.try_send(String::from_utf8_lossy(&line).into_owned());
            }
            Ok(Err(e)) => {
                failure = Some(e);
                guard.close_job();
                let _ = guard.child.kill();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if status.is_none() {
            status = guard.child.try_wait().map_err(|e| e.to_string())?;
            if status.is_some() {
                guard.close_job();
            }
        }
        if disconnected && status.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let a_result = a.join();
    let b_result = b.join();
    if a_result.is_err() || b_result.is_err() {
        return Err("Log reader panicked".into());
    }
    let status = status.ok_or("Child status unavailable")?;
    writeln!(
        file,
        "Exit: {status}; {}",
        if cancelled { "CANCELLED" } else { "FINISHED" }
    )
    .and_then(|_| file.flush())
    .map_err(|e| e.to_string())?;
    if let Some(error) = failure {
        return Err(error);
    }
    if cancelled {
        return Ok(Outcome::Cancelled);
    }
    if !status.success() {
        return Err(format!(
            "COLMAP exited with {status}; see {}",
            log.display()
        ));
    }
    Ok(Outcome::Completed)
}

// Job lifetime is owned by the worker; abnormal returns also stop its child.
struct OwnedChild {
    child: std::process::Child,
    job: windows_sys::Win32::Foundation::HANDLE,
}
impl OwnedChild {
    fn attach(child: std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        let mut owned = Self {
            child,
            job: std::ptr::null_mut(),
        };
        // SAFETY: null security/name requests an unnamed, non-inherited handle.
        owned.job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if owned.job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: valid handle, exact live struct size and valid owned process handle.
        let configured = unsafe {
            SetInformationJobObject(
                owned.job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) != 0
                && AssignProcessToJobObject(owned.job, owned.child.as_raw_handle()) != 0
        };
        if !configured {
            return Err(format!(
                "Cannot isolate child process: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(owned)
    }
    fn close_job(&mut self) {
        if !self.job.is_null() {
            // SAFETY: handle is exclusively owned and invalidated immediately.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
            self.job = std::ptr::null_mut();
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.close_job();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::Ordering, mpsc};
    fn path(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "studio-job-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.join("run.log")
    }
    #[test]
    fn captures_success_and_failure_without_ui_drain() {
        let log = path("exit");
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/D", "/C", "echo one & echo two 1>&2 & exit /b 7"]);
        let r = execute(&mut cmd, &log, Arc::new(AtomicBool::new(false)), tx);
        assert!(r.is_err());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("one") && text.contains("two") && text.contains("7"));
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/D", "/C", "echo ok"]);
        let success = log.with_file_name("success.log");
        assert_eq!(
            execute(
                &mut cmd,
                &success,
                Arc::new(AtomicBool::new(false)),
                tx.clone()
            )
            .unwrap(),
            Outcome::Completed
        );
        let mut appended = Command::new("cmd.exe");
        appended.args(["/D", "/C", "echo appended-stage"]);
        assert_eq!(
            execute_append(
                &mut appended,
                &success,
                Arc::new(AtomicBool::new(false)),
                tx.clone()
            )
            .unwrap(),
            Outcome::Completed
        );
        let combined = std::fs::read_to_string(&success).unwrap();
        assert!(combined.contains("ok") && combined.contains("appended-stage"));
        assert!(combined.contains("NEXT PIPELINE STAGE"));
        assert!(execute(&mut cmd, &success, Arc::new(AtomicBool::new(false)), tx).is_err());
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[test]
    fn capture_owns_output_and_honors_pre_cancel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "echo captured-out & echo captured-err 1>&2"]);
        let captured = capture(
            &mut command,
            cancel.clone(),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let CaptureOutcome::Completed(output) = captured else {
            panic!("capture unexpectedly cancelled");
        };
        assert!(String::from_utf8_lossy(&output.stdout).contains("captured-out"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("captured-err"));

        cancel.store(true, Ordering::SeqCst);
        let mut never_started = Command::new("nonexistent-program.exe");
        assert!(matches!(
            capture(
                &mut never_started,
                cancel,
                std::time::Duration::from_secs(5)
            )
            .unwrap(),
            CaptureOutcome::Cancelled
        ));
    }
    #[test]
    fn pre_cancel_never_spawns_or_creates_log() {
        let log = path("pre-cancel");
        let (tx, _rx) = mpsc::sync_channel(1);
        assert_eq!(
            execute(
                &mut Command::new("nonexistent-program.exe"),
                &log,
                Arc::new(AtomicBool::new(true)),
                tx
            )
            .unwrap(),
            Outcome::Cancelled
        );
        assert!(!log.exists());
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }
    #[test]
    fn cancellation_stops_child_and_retains_log() {
        let log = path("cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (tx, rx) = mpsc::sync_channel(256);
        let log2 = log.clone();
        let worker = std::thread::spawn(move || {
            let mut cmd = Command::new("powershell.exe");
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Write-Output ready; Start-Sleep -Seconds 60",
            ]);
            execute(&mut cmd, &log2, flag, tx)
        });
        let mut ready = false;
        while let Ok(line) = rx.recv_timeout(std::time::Duration::from_secs(10)) {
            if line.contains("ready") {
                ready = true;
                break;
            }
        }
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(worker.join().unwrap().unwrap(), Outcome::Cancelled);
        assert!(ready);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("CANCELLED"));
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }
}
