# Adding New Probes to HoneyBeePF

This guide outlines the steps to add a new eBPF tracepoint probe to the HoneyBeePF agent. The architecture involves three main components: data structure definition, kernel-side eBPF implementation, and userspace probe logic.

---

## 1. Define Common Data Structures
First, define the event struct that will be shared between the kernel and userspace.

**File:** `honeybeepf-common/src/lib.rs`

1.  Create a struct representing your event data.
2.  Include `EventMetadata` as the first field for standard metadata (PID, cgroup, timestamp).
3.  Implement `aya::Pod` for userspace compatibility.

```rust
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MyBuiltinEvent {
    pub metadata: EventMetadata, // Common fields (pid, cgroup_id, timestamp)
    pub my_field: u32,
    pub some_data: [u8; 16],
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for MyBuiltinEvent {}
```

---

## 2. Kernel-Side Implementation
Implement the eBPF program that attaches to the tracepoint and emits events.

**Location:** `honeybeepf-ebpf/src/probes/builtin/`

1.  **Create a new file** (e.g., `my_probe.rs`) and declare it in `mod.rs`.
2.  **Define the RingBuf map** to transport events.
3.  **Implement `HoneyBeeEvent`** for your struct.
4.  **Write the tracepoint function** using `emit_event`.

```rust
use aya_ebpf::{
    macros::{map, tracepoint},
    maps::RingBuf,
    programs::TracePointContext,
};
use honeybeepf_common::{EventMetadata, MyBuiltinEvent};
use crate::probes::{emit_event, HoneyBeeEvent};

#[map]
const MAX_EVENT_SIZE: u32 = 1024 * 1024;

pub static MY_BUILTIN_EVENTS: RingBuf = RingBuf::with_byte_size(MAX_EVENT_SIZE, 0);


// 1. Implement the trait to populate your specific fields
impl HoneyBeeEvent for MyBuiltinEvent {
    fn metadata(&mut self) -> &mut EventMetadata {
        &mut self.metadata
    }

    fn fill(&mut self, ctx: &TracePointContext) -> Result<(), u32> {
        // Init base metadata (pid, etc.) automatically
        self.init_base();
        
        // Populate your custom fields
        // Example: Reading a kernel integer argument
        self.my_field = unsafe {             
             bpf_probe_read_kernel(ptr).map_err(|_| 1u32)?
        };
        
        Ok(())
    }
}

// 2. Define the tracepoint program
#[tracepoint]
pub fn honeybeepf_my_probe(ctx: TracePointContext) -> u32 {
    // Generic helper handles reservation, filling, and submission
    emit_event::<MyBuiltinEvent>(&MY_BUILTIN_EVENTS, &ctx)
}
```

---

## 3. Userspace Implementation
Implement the userspace agent logic to attach the probe and consume events.

**Location:** `honeybeepf/src/probes/builtin/`

1.  **Create a new file** (e.g., `my_probe.rs`) and declare it in `mod.rs`.
2.  **Define a struct** for your probe (e.g., `MyBuiltinProbe`).
3.  **Implement the `Probe` trait**.

```rust
use anyhow::Result;
use aya::Bpf;
use honeybeepf_common::MyBuiltinEvent;
use log::info;
use crate::probes::{attach_tracepoint, spawn_ringbuf_handler, Probe, TracepointConfig};

pub struct MyBuiltinProbe;

impl Probe for MyBuiltinProbe {
    fn attach(&self, bpf: &mut Bpf) -> Result<()> {
        info!("Attaching my builtin probe...");

        // 1. Attach to the kernel tracepoint
        attach_tracepoint(
            bpf,
            TracepointConfig {
                program_name: "honeybeepf_my_probe", // Must match kernel function name
                category: "syscalls",                // Tracepoint category (e.g., /sys/kernel/debug/tracing/events/...)
                name: "sys_enter_openat",            // Tracepoint name
            },
        )?;

        // 2. Spawn a handler for the RingBuf
        spawn_ringbuf_handler(bpf, "MY_BUILTIN_EVENTS", |event: MyBuiltinEvent| {
            info!(
                "Event received: pid={} field={}",
                event.metadata.pid, event.my_field
            );
        })?;

        Ok(())
    }
}
```

---

## 4. Register the Probe
Finally, add your new probe to the main engine to ensure it runs.

**File:** `honeybeepf/src/lib.rs`

1.  Import your probe module.
2.  Update `HoneyBeeEngine::attach_probes` to attach your probe.
3.  (Optional) Add a feature flag in `Settings` to toggle it.

```rust
// In honeybeepf/src/lib.rs

use crate::probes::builtin::my_probe::MyBuiltinProbe;

// ...

fn attach_probes(&mut self) -> Result<()> {
    // ... existing probes ...

    // Attach your new probe
    // You can guard this with a config check if desired
    MyBuiltinProbe.attach(&mut self.bpf)?;

    Ok(())
}
```
