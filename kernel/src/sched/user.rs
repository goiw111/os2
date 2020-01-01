//! System calls and kernel <-> user mode switching...

use x86_64::{
    registers::{
        model_specific::{Efer, EferFlags, Msr},
        rflags,
    },
    structures::paging::PageTableFlags,
};

use crate::{
    cap::ResourceHandle,
    interrupts::SELECTORS,
    memory::{map_region, VirtualMemoryRegion},
};

const USER_STACK_SIZE: usize = 1; // pages

// Some MSRs used for system call handling.

/// Contains the stack and code segmets for syscall/sysret.
const STAR: Msr = Msr::new(0xC000_0081);

/// Contains the kernel rip for syscall handler.
const LSTAR: Msr = Msr::new(0xC000_0082);

/// Contains the kernel rflags mask for syscall.
const FMASK: Msr = Msr::new(0xC000_0084);

/// Allocates virtual address space, adds appropriate page table mappings, loads the specified code
/// section into the allocated memory.
///
/// Returns the virtual address region where the code has been loaded and the first RIP to start
/// executing.
pub fn load_user_code_section() -> (ResourceHandle, usize) {
    let user_code_section = VirtualMemoryRegion::alloc_with_guard(1).register(); // TODO

    // Map the code section.
    map_region(
        user_code_section,
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    );

    // TODO: load the code

    // TODO: this is test code that is an infinite loop followed by nops
    let start_addr = user_code_section.with(|cap| {
        const TEST_CODE: &[u8] = &[
            0xEB, 0xFE, // here: jmp here
            0x90, // nop
            0x90, // nop
            0x90, // nop
            0x90, // nop
            0x90, // nop
            0x90, // nop
            0x90, // nop
            0x90, // nop
        ];

        unsafe {
            let start = cap_unwrap!(VirtualMemoryRegion(cap)).start();
            for (i, b) in TEST_CODE.iter().enumerate() {
                start.offset(i as isize).write(*b);
            }
            start as usize
        }
    });

    (user_code_section, start_addr)
}

/// Allocates virtual address space for the user stack (fixed size). Adds appropriate page table
/// mappings (read/write, not execute).
///
/// Returns the virtual address region of the stack. The first and last pages are left unmapped as
/// guard pages. The stack should be used from the end (high-addresses) of the region (top of
/// stack), since it grows downward.
pub fn allocate_user_stack() -> ResourceHandle {
    // Allocate the stack the user will run on.
    let user_stack = VirtualMemoryRegion::alloc_with_guard(USER_STACK_SIZE).register();

    // Map the stack into the address space.
    map_region(
        user_stack,
        PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE,
    );

    user_stack
}

/// Set some MSRs, registers to enable syscalls and user/kernel context switching.
pub fn init() {
    unsafe {
        // Need to set IA32_EFER.SCE
        Efer::update(|flags| *flags |= EferFlags::SYSTEM_CALL_EXTENSIONS);

        // STAR: Ring 0 and Ring 3 segments
        // - Kernel mode CS is bits 47:32
        // - Kernel mode SS is bits 47:32 + 8
        // - User mode CS is bits 63:48 + 16
        // - User mode SS is bits 63:48 + 8
        let selectors = SELECTORS.lock();
        let kernel_base: u16 = selectors.kernel_cs.index() * 8;
        printk!("k {} u {}", kernel_base, selectors.user_ss.index());
        let user_base: u16 = selectors.user_ss.index() * 8 - 8;
        let star: u64 = ((kernel_base as u64) << 32) | ((user_base as u64) << 48);
        STAR.write(star);

        // LSTAR: Syscall Entry RIP
        LSTAR.write(handle_syscall as u64);

        // FMASK: rflags mask: any set bits are cleared on syscall
        FMASK.write(0);
    }
}

/// Switch to user mode, executing the given code with the given address.
pub fn switch_to_user(code: (ResourceHandle, usize), stack: ResourceHandle) -> ! {
    // Compute new register values
    let rsp = stack.with(|cap| {
        let region = cap_unwrap!(VirtualMemoryRegion(cap));
        let start = region.start();
        let len = region.len();
        unsafe { start.offset(len as isize) }
    });

    let (_handle, rip) = code;

    let rflags = (rflags::read() | rflags::RFlags::INTERRUPT_FLAG).bits();

    // TODO: save kernel stack location somewhere so that we can switch back to it during an
    // interrupt. Or do we need to? The scheduler already knows where its two stacks are... can we
    // just wipe one of them and use it?

    // https://software.intel.com/sites/default/files/managed/39/c5/325462-sdm-vol-1-2abcd-3abcd.pdf#G43.25974
    //
    // Set the following and execute the `sysret` instruction:
    // - user rip: load into rcx before sysret
    // - rflags: load into r11 before sysret
    // - also want to set any register values to be given to the user
    //      - user rsp
    //      - clear all other regs
    //
    // TODO: eventually we may want to have a general mechanism for restoring registers to know
    // values from a struct or something. For now, we just clear all registers.
    unsafe {
        asm!(
            "
            # needed for sysret
            mov $0, %rcx
            mov $1, %r11

            # clear other regs
            xor %rax, %rax
            xor %rbx, %rbx
            xor %rdx, %rdx
            xor %rdi, %rdi
            xor %rsi, %rsi
            xor %r8 , %r8
            xor %r9 , %r9
            xor %r10, %r10
            xor %r12, %r12
            xor %r13, %r13
            xor %r14, %r14
            xor %r15, %r15

            # disable interrupts before loading the user stack; otherwise, an interrupt may be
            # serviced on the wrong stack.
            cli

            # no more stack refs until sysret
            mov $2, %rsp

            # return to usermode (ring 3)
            sysret
            "
            : /* no outputs */
            : "r"(rip), "r"(rflags), "r"(rsp)
            : "rcx", "r1", "memory"
            : "volatile"
        );
    }

    unreachable!();
}

/// Handle a `syscall` instruction
#[naked]
extern "C" fn handle_syscall() {
    // TODO: switch to kernel stack, save user regs
    //
    // https://software.intel.com/sites/default/files/managed/39/c5/325462-sdm-vol-1-2abcd-3abcd.pdf#G43.25974
    //
    // TODO: for syscall handling: see the warnings at the end of the above chapter in the Intel
    // SDM (e.g. regarding interrupts, user stack)

    todo!("syscall");
}
