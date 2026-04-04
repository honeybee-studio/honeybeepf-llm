#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
pub struct task_struct {
    // NOTE: This is a simplified subset of task_struct fields and is not ABI-stable.
    // Do not assume this layout or offsets match real kernel task_struct across
    // different kernel versions/configurations; for kprobe/field access, prefer
    // BTF/CO-RE generated layouts.
    pub pid: i32,
    pub tgid: i32,
    pub exit_code: i32,
    pub start_time: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
pub struct oom_control {
    // NOTE: Simplified layout matching common kernel versions.
    // The `chosen` field points to the victim task_struct selected by the OOM killer.
    // Prefer BTF/CO-RE (aya-tool generate) for production use across kernel versions.
    pub oom_zonelist: u64, // struct zonelist *
    pub nodemask: u64,     // nodemask_t *
    pub memcg: u64,        // struct mem_cgroup *
    pub gfp_mask: u32,
    pub order: i32,
    pub totalpages: u64,
    pub chosen: u64, // struct task_struct * (victim)
}
