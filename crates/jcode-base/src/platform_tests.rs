use super::*;

#[test]
fn desired_nofile_soft_limit_only_raises_when_possible() {
    assert_eq!(desired_nofile_soft_limit(1024, 524_288, 8192), Some(8192));
    assert_eq!(desired_nofile_soft_limit(8192, 524_288, 8192), None);
    assert_eq!(desired_nofile_soft_limit(1024, 4096, 8192), Some(4096));
}

#[cfg(unix)]
#[test]
fn spawn_detached_creates_new_session() {
    use tempfile::NamedTempFile;

    let output = NamedTempFile::new().expect("temp file");
    let output_path = output.path().to_string_lossy().to_string();
    let parent_sid = unsafe { libc::getsid(0) };

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg("ps -o sid= -p $$ > \"$JCODE_TEST_OUTPUT\"")
        .env("JCODE_TEST_OUTPUT", &output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = super::spawn_detached(&mut cmd).expect("spawn detached child");
    let status = child.wait().expect("wait for child");
    assert!(status.success(), "child should exit successfully");

    let child_sid = std::fs::read_to_string(&output_path)
        .expect("read child sid")
        .trim()
        .parse::<u32>()
        .expect("parse child sid");

    assert_eq!(
        child_sid,
        child.id(),
        "detached child should lead its own session"
    );
    assert_ne!(
        child_sid as i32, parent_sid,
        "detached child should not share parent session"
    );
}

#[cfg(windows)]
#[test]
fn is_process_running_reports_exited_children_as_stopped() {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", "ping -n 3 127.0.0.1 >NUL"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().expect("spawn child");
    let pid = child.id();
    assert!(
        super::is_process_running(pid),
        "child should initially be running"
    );

    let status = child.wait().expect("wait for child");
    assert!(status.success(), "child should exit successfully");
    std::thread::sleep(Duration::from_millis(100));

    assert!(
        !super::is_process_running(pid),
        "exited child should not be reported as running"
    );
}

#[cfg(windows)]
#[test]
fn signal_detached_process_group_terminates_descendant_tree() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().expect("temp dir");
    let ready_path = temp.path().join("child-ready.txt");
    let survived_path = temp.path().join("child-survived.txt");
    let child_script_path = temp.path().join("child.cmd");
    let parent_script_path = temp.path().join("parent.cmd");
    let child_script = concat!(
        "@echo off\r\n",
        "echo ready>\"%~dp0child-ready.txt\"\r\n",
        "ping -n 6 127.0.0.1 >NUL\r\n",
        "echo survived>\"%~dp0child-survived.txt\"\r\n"
    );
    let parent_script = concat!(
        "@echo off\r\n",
        "start \"\" /B cmd.exe /D /C \"\"%~dp0child.cmd\"\"\r\n",
        "ping -n 30 127.0.0.1 >NUL\r\n"
    );
    std::fs::write(&child_script_path, child_script).expect("write child command script");
    std::fs::write(&parent_script_path, parent_script).expect("write parent command script");
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/D", "/C"])
        .arg(&parent_script_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut parent = super::spawn_detached(&mut cmd).expect("spawn detached process tree");
    let parent_pid = parent.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready_path.exists(), "descendant should report ready");
    assert!(super::is_process_running(parent_pid));

    super::signal_detached_process_group(parent_pid, 0).expect("terminate process tree");
    let deadline = Instant::now() + Duration::from_secs(10);
    while super::is_process_running(parent_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = parent.wait();

    assert!(!super::is_process_running(parent_pid), "parent should stop");
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        !survived_path.exists(),
        "descendant should not survive termination of the detached process tree"
    );
}

#[cfg(windows)]
#[test]
fn spawn_replacement_process_returns_without_waiting_for_child_exit() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", "ping -n 4 127.0.0.1 >NUL"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let start = Instant::now();
    let mut child = super::spawn_replacement_process(&mut cmd)
        .expect("spawn replacement process should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "replacement spawn should not block, took {:?}",
        elapsed
    );
    assert!(
        child.try_wait().expect("poll child status").is_none(),
        "replacement child should still be running immediately after spawn"
    );

    child.kill().ok();
    let _ = child.wait();
}

#[cfg(windows)]
#[test]
fn spawn_detached_no_window_allocates_no_console() {
    // Regression: DETACHED_PROCESS leaves a console-subsystem child (cmd.exe,
    // python.exe) with no console to inherit, so Windows allocates a BRAND NEW
    // one. The user sees a console window flash and steal focus. Observer hooks
    // fire on every tool call, so this flashed constantly on Windows.
    //
    // CREATE_NO_WINDOW is IGNORED when combined with DETACHED_PROCESS, so it
    // must be used INSTEAD of it. It also leaves the child without an inherited
    // console, which is the detachment the background-hook path wants.
    //
    // The console window is owned by a SEPARATE conhost/OpenConsole process
    // whose PARENT is the child, so neither per-child-PID enumeration nor a
    // global before/after diff works: the former never sees it, the latter
    // races other tests spawning processes in parallel. Attribute each console
    // window to our child via the owning process's parent PID.
    use std::process::{Command, Stdio};

    // windows-sys is built here without the Win32_UI feature.
    type EnumProc = unsafe extern "system" fn(isize, isize) -> i32;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: EnumProc, lparam: isize) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    }

    /// Console windows on the desktop, as (owner pid, visible).
    unsafe extern "system" fn collect(hwnd: isize, lparam: isize) -> i32 {
        let out = unsafe { &mut *(lparam as *mut Vec<(u32, bool)>) };
        let mut class = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), 256) };
        if len > 0 {
            let name = String::from_utf16_lossy(&class[..len as usize]);
            if name.contains("ConsoleWindow") {
                let mut pid: u32 = 0;
                unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
                let visible = unsafe { IsWindowVisible(hwnd) } != 0;
                out.push((pid, visible));
            }
        }
        1
    }

    // windows-sys here also lacks Win32_System_Diagnostics; declare the
    // ToolHelp snapshot API and the prefix of PROCESSENTRY32 we read.
    #[repr(C)]
    struct ProcessEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u8; 260],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32First(snap: isize, entry: *mut ProcessEntry32) -> i32;
        fn Process32Next(snap: isize, entry: *mut ProcessEntry32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const INVALID_HANDLE: isize = -1;

    /// Parent PID of `pid`, via a process snapshot.
    fn parent_pid(pid: u32) -> Option<u32> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE {
                return None;
            }
            let mut entry: ProcessEntry32 = std::mem::zeroed();
            entry.dw_size = std::mem::size_of::<ProcessEntry32>() as u32;
            let mut found = None;
            if Process32First(snap, &mut entry) != 0 {
                loop {
                    if entry.th32_process_id == pid {
                        found = Some(entry.th32_parent_process_id);
                        break;
                    }
                    if Process32Next(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
            found
        }
    }
    /// Visible console windows hosted on behalf of `child_pid`.
    fn console_windows_for_child(child_pid: u32) -> usize {
        let mut all: Vec<(u32, bool)> = Vec::new();
        unsafe { EnumWindows(collect, &mut all as *mut Vec<(u32, bool)> as isize) };
        all.into_iter()
            .filter(|(owner, visible)| {
                *visible
                    && (*owner == child_pid || parent_pid(*owner) == Some(child_pid))
            })
            .count()
    }

    // cmd.exe reproduces the symptom and lives long enough to be observed, with
    // the same all-null stdio the observer-hook path uses.
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", "ping -n 4 127.0.0.1 >NUL"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child =
        super::spawn_detached_no_window(&mut cmd).expect("spawn detached no-window child");

    let mut seen = 0usize;
    for _ in 0..20 {
        seen = seen.max(console_windows_for_child(child.id()));
        if seen > 0 || child.try_wait().expect("poll child").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    child.kill().ok();
    let _ = child.wait();

    assert_eq!(
        seen, 0,
        "spawning a detached background child must not create a visible console \
         window (saw {seen}); DETACHED_PROCESS allocates one and makes a window \
         flash on the user's desktop on every hook invocation"
    );
}
