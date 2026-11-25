/// Per-thread CPU time used by the codec counters on the supported platforms.
#[cfg(windows)]
pub fn thread_cpu_ns() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn GetThreadTimes(
            t: *mut std::ffi::c_void,
            create: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
    }
    let (mut create, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
    unsafe {
        if GetThreadTimes(
            GetCurrentThread(),
            &mut create,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return 0;
        }
    }
    (kernel + user) * 100
}
#[cfg(target_os = "linux")]
pub fn thread_cpu_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) != 0 {
            return 0;
        }
    }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn thread_cpu_ns() -> u64 {
    0
}

#[cfg(windows)]
pub fn thread_cpu_cycles() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn QueryThreadCycleTime(t: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
    }
    let mut cycles = 0;
    unsafe {
        QueryThreadCycleTime(GetCurrentThread(), &mut cycles);
    }
    cycles
}
#[cfg(not(windows))]
pub fn thread_cpu_cycles() -> u64 {
    0
}
