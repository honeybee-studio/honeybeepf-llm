/// Common tracepoint header for syscall events (sys_enter_*, sys_exit_*)
#[repr(C)]
#[allow(dead_code)]
pub struct SyscallTraceHeader {
    pub common_type: u16,
    pub common_flags: u8,
    pub common_preempt_count: u8,
    pub common_pid: i32,
    pub __syscall_nr: i32,
    pub _pad: i32,
}

// ============================================================
// sys_enter_openat / sys_exit_openat
// ============================================================

#[repr(C)]
#[allow(dead_code)]
pub struct SysEnterOpenat {
    pub header: SyscallTraceHeader,
    pub dfd: i64,
    pub filename: u64,
    pub flags: i64,
    pub mode: i64,
}

#[repr(C)]
#[allow(dead_code)]
pub struct SysExitOpenat {
    pub header: SyscallTraceHeader,
    pub ret: i64,
}

// ============================================================
// sys_enter_close
// ============================================================

#[repr(C)]
#[allow(dead_code)]
pub struct SysEnterClose {
    pub header: SyscallTraceHeader,
    pub fd: i64,
}
