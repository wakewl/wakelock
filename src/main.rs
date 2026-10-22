// wakelock: lock screen for ext-session-lock-v1; returns once the lock is confirmed. A supervisor
// restarts the lock surface if it crashes, since the compositor keeps a session locked with
// nothing to unlock it when the lock client dies.
mod pam;
mod ppm;
mod surface;

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::DirBuilderExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const USAGE: &str = "usage: wakelock [--pam-service NAME] [--pam-dir DIR]";

pub struct Options {
    pub pam_service: String,
    pub pam_dir: Option<String>,
}

impl Options {
    fn args(&self) -> Vec<String> {
        let mut args = vec!["--pam-service".to_owned(), self.pam_service.clone()];
        if let Some(dir) = &self.pam_dir {
            args.extend(["--pam-dir".to_owned(), dir.clone()]);
        }
        args
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn display_name() -> String {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    display.rsplit('/').next().unwrap_or("wayland-0").to_owned()
}

fn wayland_socket() -> PathBuf {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    if display.starts_with('/') { PathBuf::from(display) } else { runtime_dir().join(display) }
}

fn socket_path() -> PathBuf {
    runtime_dir().join(format!("wakelock-{}.sock", display_name()))
}

pub fn snapshot_dir() -> PathBuf {
    runtime_dir().join(format!("wakelock-{}", display_name()))
}

fn query() -> Option<String> {
    let mut stream = UnixStream::connect(socket_path()).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply.trim().to_owned())
}

fn wait_until_locked(timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        match query().as_deref() {
            Some("locked") => return true,
            None => return false,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    false
}

fn lock(options: &Options) -> ExitCode {
    match query().as_deref() {
        Some("locked") => return ExitCode::SUCCESS,
        Some(_) => {
            return if wait_until_locked(Duration::from_secs(5)) {
                ExitCode::SUCCESS
            } else {
                eprintln!("wakelock: the lock screen did not lock the session");
                ExitCode::FAILURE
            };
        }
        None => {}
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("wakelock: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut command = Command::new(exe);
    command.arg("--supervise").args(options.args()).stdin(Stdio::null()).stdout(Stdio::piped());
    // Outlive whatever started us
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut supervisor = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("wakelock: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = supervisor.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line.trim().to_owned());
    });
    match receiver.recv_timeout(Duration::from_secs(5)).as_deref() {
        Ok("locked") => ExitCode::SUCCESS,
        _ => {
            eprintln!("wakelock: the lock screen did not lock the session");
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            let _ = std::fs::remove_file(socket_path());
            let _ = std::fs::remove_dir_all(snapshot_dir());
            ExitCode::FAILURE
        }
    }
}

fn supervise(options: &Options) -> ExitCode {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        return ExitCode::SUCCESS;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("wakelock: cannot listen on {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let locked = Arc::new(AtomicBool::new(false));
    {
        let locked = locked.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let state = if locked.load(Ordering::SeqCst) { "locked\n" } else { "locking\n" };
                let _ = stream.write_all(state.as_bytes());
            }
        });
    }

    let snapshots = snapshot_dir();
    let _ = std::fs::remove_dir_all(&snapshots);
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&snapshots);
    let cleanup = || {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&snapshots);
    };

    let exe = std::env::current_exe().expect("own path");
    let mut first = true;
    loop {
        let mut command = Command::new(&exe);
        command.arg("--surface").args(options.args()).stdin(Stdio::null()).stdout(Stdio::piped());
        if first {
            command.arg("--snapshot");
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("wakelock: {e}");
                cleanup();
                return ExitCode::FAILURE;
            }
        };
        let stdout = child.stdout.take().unwrap();
        {
            let locked = locked.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line == "locked" && !locked.swap(true, Ordering::SeqCst) {
                        let mut out = std::io::stdout();
                        let _ = writeln!(out, "locked");
                        let _ = out.flush();
                    }
                }
            });
        }
        let status = child.wait();
        first = false;
        if status.as_ref().is_ok_and(|s| s.success()) {
            cleanup();
            return ExitCode::SUCCESS;
        }
        // Only restart if the session is left locked
        if !locked.load(Ordering::SeqCst) {
            eprintln!("wakelock: the lock screen exited before locking ({status:?})");
            cleanup();
            return ExitCode::FAILURE;
        }
        if !wayland_socket().exists() {
            cleanup();
            return ExitCode::SUCCESS;
        }
        eprintln!("wakelock: the lock screen exited ({status:?}), starting it again");
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() -> ExitCode {
    let mut options = Options { pam_service: "login".into(), pam_dir: None };
    let mut mode = None;
    let mut snapshot = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pam-service" => match args.next() {
                Some(name) => options.pam_service = name,
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                }
            },
            "--pam-dir" => options.pam_dir = args.next(),
            "--supervise" | "--surface" => mode = Some(arg),
            "--snapshot" => snapshot = true,
            "-V" | "--version" => {
                println!("wakelock {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    match mode.as_deref() {
        None => lock(&options),
        Some("--supervise") => supervise(&options),
        _ => surface::run(&options, snapshot),
    }
}
