#![no_std]
#![no_main]
// For the asm! clamp in the plaintext receive path (bpfel has no stable inline asm).
#![feature(asm_experimental_arch)]

mod probes;

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
