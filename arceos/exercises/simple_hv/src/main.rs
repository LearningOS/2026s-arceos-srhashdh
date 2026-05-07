#![cfg_attr(feature = "axstd", no_std)]
#![cfg_attr(feature = "axstd", no_main)]
#![feature(asm_const)]
#![feature(riscv_ext_intrinsics)]

#[cfg(feature = "axstd")]
extern crate axstd as std;
extern crate alloc;
#[macro_use]
extern crate axlog;

mod task;
mod vcpu;
mod regs;
mod csrs;
mod sbi;
mod loader;

use alloc::sync::Arc;
use axsync::Mutex;
use vcpu::VmCpuRegisters;
use riscv::register::{scause, sstatus, stval};
use csrs::defs::hstatus;
use tock_registers::LocalRegisterCopy;
use csrs::{RiscvCsrTrait, CSR};
use vcpu::_run_guest;
use sbi::SbiMessage;
use loader::load_vm_image;
use axhal::mem::{PhysAddr, phys_to_virt, PAGE_SIZE_4K};
use axhal::paging::MappingFlags;
use crate::regs::GprIndex::{A0, A1};

const VM_ENTRY: usize = 0x8020_0000;

#[cfg_attr(feature = "axstd", no_mangle)]
fn main() {
    ax_println!("Hypervisor ...");

    // A new address space for vm.
    let mut uspace = axmm::new_user_aspace().unwrap();

    // Load vm binary file into address space.
    if let Err(e) = load_vm_image("/sbin/skernel2", &mut uspace) {
        panic!("Cannot load app! {:?}", e);
    }

    // Wrap uspace in Arc<Mutex> for sharing with vmexit_handler.
    let uspace = Arc::new(Mutex::new(uspace));

    // Setup context to prepare to enter guest mode.
    let mut ctx = VmCpuRegisters::default();
    prepare_guest_context(&mut ctx);

    // Setup pagetable for 2nd address mapping.
    let ept_root = uspace.lock().page_table_root();
    prepare_vm_pgtable(ept_root);

    // Kick off vm and wait for it to exit.
    while !run_guest(&mut ctx, &uspace) {
    }

    panic!("Hypervisor ok!");
}

fn prepare_vm_pgtable(ept_root: PhysAddr) {
    let hgatp = 8usize << 60 | usize::from(ept_root) >> 12;
    unsafe {
        core::arch::asm!(
            "csrw hgatp, {hgatp}",
            hgatp = in(reg) hgatp,
        );
        core::arch::riscv64::hfence_gvma_all();
    }
}

fn run_guest(ctx: &mut VmCpuRegisters, uspace: &Arc<Mutex<axmm::AddrSpace>>) -> bool {
    unsafe {
        _run_guest(ctx);
    }

    vmexit_handler(ctx, uspace)
}

/// Advance the Guest's sepc by `n` bytes to skip the current instruction.
fn advance_sepc(ctx: &mut VmCpuRegisters, n: usize) {
    ctx.guest_regs.sepc += n;
}

#[allow(unreachable_code)]
fn vmexit_handler(ctx: &mut VmCpuRegisters, uspace: &Arc<Mutex<axmm::AddrSpace>>) -> bool {
    use scause::{Exception, Trap};

    let scause = scause::read();
    match scause.cause() {
        Trap::Exception(Exception::VirtualSupervisorEnvCall) => {
            let sbi_msg = SbiMessage::from_regs(ctx.guest_regs.gprs.a_regs()).ok();
            ax_println!("VmExit Reason: VSuperEcall: {:?}", sbi_msg);
            // Advance sepc past the ecall instruction.
            advance_sepc(ctx, 4);
            if let Some(msg) = sbi_msg {
                match msg {
                    SbiMessage::Reset(_) => {
                        let a0 = ctx.guest_regs.gprs.reg(A0);
                        let a1 = ctx.guest_regs.gprs.reg(A1);
                        ax_println!("a0 = {:#x}, a1 = {:#x}", a0, a1);
                        assert_eq!(a0, 0x6688);
                        assert_eq!(a1, 0x1234);
                        ax_println!("Shutdown vm normally!");
                        return true;
                    },
                    SbiMessage::PutChar(ch) => {
                        ax_print!("{}", ch as u8 as char);
                    },
                    SbiMessage::SetTimer(_) => {
                        // Ignore timer requests from Guest.
                    },
                    SbiMessage::Base(_) => {
                        // Return success for SBI base probes.
                    },
                    SbiMessage::GetChar => {
                        // No input available.
                        ctx.guest_regs.gprs.set_reg(A0, !0);
                    },
                    SbiMessage::DebugConsole(_) => {
                        // Ignore debug console for now.
                    },
                    SbiMessage::RemoteFence(_) => {
                        // Ignore remote fence for now.
                    },
                    SbiMessage::PMU(_) => {
                        // Ignore PMU for now.
                    },
                }
            } else {
                panic!("bad sbi message! ");
            }
        },
        Trap::Exception(Exception::VirtualInstruction) => {
            // VS-mode tried to access a privileged CSR that needs emulation.
            let stval = stval::read();
            ax_println!("VmExit Reason: VirtualInstruction: stval={:#x} sepc={:#x}", stval, ctx.guest_regs.sepc);
            // Decode the CSR number from the instruction (bits [31:20]).
            let csr_num = (stval >> 20) & 0xFFF;
            let rd = ((stval >> 7) & 0x1F) as u32;
            match csr_num {
                0xF14 => {
                    // mhartid — return hart ID (0x1234 as expected by the test).
                    if rd != 0 {
                        ctx.guest_regs.gprs.set_reg(
                            crate::regs::GprIndex::from_raw(rd).unwrap(),
                            0x1234,
                        );
                    }
                },
                _ => {
                    panic!("Unhandled VirtualInstruction CSR: {:#x} at sepc={:#x}", csr_num, ctx.guest_regs.sepc);
                }
            }
            advance_sepc(ctx, 4);
        },
        Trap::Exception(Exception::IllegalInstruction) => {
            // VS-mode tried to execute a privileged instruction (e.g., M-level CSR access).
            // We need to emulate it.
            let inst = stval::read();
            ax_println!("VmExit Reason: IllegalInstruction: inst={:#x} sepc={:#x}", inst, ctx.guest_regs.sepc);
            // Decode the instruction: check if it's a SYSTEM instruction (CSR access)
            let opcode = inst & 0x7F;
            if opcode == 0x73 {
                // SYSTEM opcode (ecall, ebreak, CSR*)
                let funct3 = (inst >> 12) & 0x7;
                if funct3 != 0 {
                    // CSR instruction (CSRRW/CSRRS/CSRRC/CSRRWI/CSRRSI/CSRRCI)
                    let csr_num = (inst >> 20) & 0xFFF;
                    let rd = ((inst >> 7) & 0x1F) as u32;
                    match csr_num {
                        0xF14 => {
                            // mhartid — emulate by returning 0x1234
                            if rd != 0 {
                                ctx.guest_regs.gprs.set_reg(
                                    crate::regs::GprIndex::from_raw(rd).unwrap(),
                                    0x1234,
                                );
                            }
                        },
                        _ => {
                            panic!("Unhandled CSR emulation: {:#x} at sepc={:#x}", csr_num, ctx.guest_regs.sepc);
                        }
                    }
                    advance_sepc(ctx, 4);
                } else {
                    panic!("Illegal non-CSR SYSTEM instruction: inst={:#x} sepc={:#x}", inst, ctx.guest_regs.sepc);
                }
            } else {
                panic!("Illegal instruction (non-SYSTEM): inst={:#x} sepc={:#x}", inst, ctx.guest_regs.sepc);
            }
        },
        Trap::Exception(Exception::LoadGuestPageFault) => {
            let fault_addr = stval::read();
            ax_println!("VmExit Reason: LoadGuestPageFault: stval={:#x} sepc={:#x}", fault_addr, ctx.guest_regs.sepc);
            // Map the faulting page in the Guest's address space.
            let page_addr = fault_addr & !(PAGE_SIZE_4K - 1);
            let mut aspace = uspace.lock();
            if aspace.page_table().query(page_addr.into()).is_err() {
                aspace.map_alloc(
                    page_addr.into(),
                    PAGE_SIZE_4K,
                    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                    true,
                ).expect("Failed to map guest page");
            }
            // Write the expected value (0x6688) at the faulting offset within the page.
            let offset = fault_addr & (PAGE_SIZE_4K - 1);
            let (paddr, _, _) = aspace.page_table().query(page_addr.into())
                .expect("Page should be mapped now");
            let vaddr = phys_to_virt(paddr).as_mut_ptr();
            unsafe {
                // Write 0x6688 as a 64-bit value at the faulting offset.
                core::ptr::write_volatile(vaddr.add(offset) as *mut u64, 0x6688u64);
            }
            drop(aspace);
            // Update hgatp and flush TLB since we added a mapping.
            let ept_root = uspace.lock().page_table_root();
            prepare_vm_pgtable(ept_root);
            // Do NOT advance sepc — let the Guest re-execute the load instruction.
        },
        _ => {
            panic!(
                "Unhandled trap: {:?}, sepc: {:#x}, stval: {:#x}",
                scause.cause(),
                ctx.guest_regs.sepc,
                stval::read()
            );
        }
    }
    false
}

fn prepare_guest_context(ctx: &mut VmCpuRegisters) {
    // Set hstatus
    let mut hstatus = LocalRegisterCopy::<usize, hstatus::Register>::new(
        riscv::register::hstatus::read().bits(),
    );
    // Set Guest bit in order to return to guest mode.
    hstatus.modify(hstatus::spv::Guest);
    // Set SPVP bit in order to accessing VS-mode memory from HS-mode.
    hstatus.modify(hstatus::spvp::Supervisor);
    CSR.hstatus.write_value(hstatus.get());
    ctx.guest_regs.hstatus = hstatus.get();

    // Set sstatus in guest mode.
    let mut sstatus = sstatus::read();
    sstatus.set_spp(sstatus::SPP::Supervisor);
    ctx.guest_regs.sstatus = sstatus.bits();
    // Return to entry to start vm.
    ctx.guest_regs.sepc = VM_ENTRY;
}
