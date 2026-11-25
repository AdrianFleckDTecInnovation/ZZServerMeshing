//! Cumulative user and kernel CPU time for differentiating work from elapsed time.
#[cfg(windows)]
fn nanos(process: bool) -> u64 {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentThread() -> *mut c_void;
        fn GetProcessTimes(
            h: *mut c_void,
            c: *mut u64,
            e: *mut u64,
            k: *mut u64,
            u: *mut u64,
        ) -> i32;
        fn GetThreadTimes(
            h: *mut c_void,
            c: *mut u64,
            e: *mut u64,
            k: *mut u64,
            u: *mut u64,
        ) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = (0, 0, 0, 0);
    // FILETIME comprises two 32-bit words. These aligned u64s have the same
    // size and layout on Windows and receive the native 100 ns counts.
    let ok = unsafe {
        if process {
            GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u)
        } else {
            GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u)
        }
    };
    if ok == 0 {
        0
    } else {
        (k + u) * 100
    }
}
#[cfg(target_os = "linux")]
fn nanos(process: bool) -> u64 {
    let clock = if process {
        libc::CLOCK_PROCESS_CPUTIME_ID
    } else {
        libc::CLOCK_THREAD_CPUTIME_ID
    };
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut t) } == 0 {
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    } else {
        0
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
fn nanos(_: bool) -> u64 {
    0
}
pub fn process_ns() -> u64 {
    nanos(true)
}
pub(crate) fn thread_ns() -> u64 {
    nanos(false)
}
