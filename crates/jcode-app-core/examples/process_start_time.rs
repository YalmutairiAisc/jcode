//! Does current_process_start_time() actually work on Windows?
//!
//! The unit tests feed synthetic timestamps. This checks the real syscall:
//! GetProcessTimes + the 1601->1970 FILETIME conversion must produce a value
//! close to "now", not 1601 or a garbage epoch. A wrong conversion here would
//! make every same-path candidate look newer (or never newer) forever.

fn main() {
    // Mirror of the private helper in server/util.rs.
    use std::os::windows::raw::HANDLE;

    #[allow(non_camel_case_types)]
    type FILETIME = [u32; 2];

    unsafe extern "system" {
        fn GetCurrentProcess() -> HANDLE;
        fn GetProcessTimes(
            process: HANDLE,
            creation: *mut FILETIME,
            exit: *mut FILETIME,
            kernel: *mut FILETIME,
            user: *mut FILETIME,
        ) -> i32;
    }

    let mut creation: FILETIME = [0, 0];
    let mut exit: FILETIME = [0, 0];
    let mut kernel: FILETIME = [0, 0];
    let mut user: FILETIME = [0, 0];
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    assert!(ok != 0, "GetProcessTimes failed");

    let ticks = ((creation[1] as u64) << 32) | (creation[0] as u64);
    const TICKS_1601_TO_1970: u64 = 116_444_736_000_000_000;
    let unix_ticks = ticks.checked_sub(TICKS_1601_TO_1970).expect("underflow");
    let started = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(unix_ticks * 100);

    let now = std::time::SystemTime::now();
    let age = now.duration_since(started).expect("start must precede now");

    println!("process start epoch secs: {}", unix_ticks / 10_000_000);
    println!("now epoch secs          : {}",
        now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    println!("age                     : {:?}", age);

    assert!(age.as_secs() < 300, "start time implausibly old: {age:?}");
    println!("\nPASS: start time is real and within seconds of now");
}
