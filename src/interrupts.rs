//! Прерывания: GDT+TSS (double-fault IST), IDT, PIC (8259), PIT (таймер).
//!
//! После `init()` прерывания включены (sti). IRQ0 (таймер ~100 Гц) ведёт на
//! asm-обработчик планировщика, все остальные IRQ замаскированы (клавиатура
//! остаётся опрашиваемой). Исключения печатают сообщение и останавливают
//! систему.

use alloc::boxed::Box;
use core::arch::asm;
use x86_64::instructions::port::Port;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::PrivilegeLevel;
use x86_64::VirtAddr;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::set_general_handler;

pub const KERNEL_CS: u16 = 0x08; // index 1
pub const KERNEL_DS: u16 = 0x10; // index 2
// User segments: index 3/4 в GDT, DPL=3; селекторы с RPL=3 (обязательно для
// iretq: целевое кольцо берётся из RPL селектора CS/SS).
pub const USER_CS: u16 = 0x1B; // index 3 | RPL 3
pub const USER_DS: u16 = 0x23; // index 4 | RPL 3

const TIMER_IRQ_VECTOR: u8 = 0x20;
const KEYBOARD_IRQ_VECTOR: u8 = 0x21;

static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut DOUBLE_FAULT_STACK: [u8; 16384] = [0; 16384];

fn init_gdt() {
    unsafe {
        TSS.interrupt_stack_table[0] = VirtAddr::new(DOUBLE_FAULT_STACK.as_ptr() as u64 + 16384);
        let gdt = Box::leak(Box::new(GlobalDescriptorTable::new()));
        let code = gdt.add_entry(Descriptor::kernel_code_segment());
        let data = gdt.add_entry(Descriptor::kernel_data_segment());
        let ucode = gdt.add_entry(Descriptor::user_code_segment());
        let udata = gdt.add_entry(Descriptor::user_data_segment());
        let tss = gdt.add_entry(Descriptor::tss_segment(&TSS));
        gdt.load();
        CS::set_reg(SegmentSelector::new(1, PrivilegeLevel::Ring0));
        let ds = SegmentSelector::new(2, PrivilegeLevel::Ring0);
        DS::set_reg(ds);
        ES::set_reg(ds);
        FS::set_reg(ds);
        GS::set_reg(ds);
        SS::set_reg(ds);
        asm!("ltr {0:x}", in(reg) tss.0, options(nomem, nostack));
        let _ = (code, data, ucode, udata);
    }
}

/// Устанавливает стек ring 0 (RSP0 в TSS), который CPU загружает при переходе
/// ring 3 -> ring 0 (прерывания, исключения, int 0x80 из пользовательской
/// задачи). Планировщик переключает его перед iretq в user-режим.
pub fn set_tss_rsp0(addr: u64) {
    unsafe {
        TSS.privilege_stack_table[0] = VirtAddr::new(addr);
    }
}

fn exception_handler(frame: InterruptStackFrame, index: u8, _err: Option<u64>) {
    // Полный дамп в serial ДО VGA-печати (VGA не видно в headless-прогонax).
    crate::vga::serial_write_atomic("\n[exc] v=0x");
    serial_hex_u64(index as u64);
    crate::vga::serial_write_atomic(" cs=");
    serial_hex_u64(frame.code_segment);
    crate::vga::serial_write_atomic(" rip=");
    serial_hex_u64(frame.instruction_pointer.as_u64());
    crate::vga::serial_write_atomic(" rsp=");
    serial_hex_u64(frame.stack_pointer.as_u64());
    crate::vga::serial_write_atomic(" flg=");
    serial_hex_u64(frame.cpu_flags);
    if let Some(err) = _err {
        crate::vga::serial_write_atomic(" err=");
        serial_hex_u64(err);
    }
    let cr2 = x86_64::registers::control::Cr2::read().as_u64();
    crate::vga::serial_write_atomic(" cr2=");
    serial_hex_u64(cr2);
    crate::vga::serial_write_atomic(" task=");
    let tname = crate::scheduler::current_name();
    let mut tn = 0usize;
    for &c in tname.iter() {
        if c == 0 { break; }
        tn += 1;
    }
    crate::vga::serial_write_atomic(core::str::from_utf8(&tname[..tn]).unwrap_or("?"));
    crate::vga::serial_write_atomic("\n");
    let mut w = crate::vga::Writer::new();
    w.write_string("EXCEPTION vector 0x");
    write_hex(&mut w, index as u64);
    w.write_string(" in task ");
    let name = crate::scheduler::current_name();
    write_bytes(&mut w, &name);

    // Исключение в коде user-задачи: убиваем задачу и возвращаемся к шеллу.
    // В кольце 0 (ядро) исключение фатально.
    if frame.code_segment == USER_CS as u64 {
        w.write_string(" (user mode) RIP=");
        write_hex(&mut w, frame.instruction_pointer.as_u64());
        if let Some(err) = _err {
            w.write_string(" ERR=");
            write_hex(&mut w, err);
        }
        w.write_string(" RSP=");
        write_hex(&mut w, frame.stack_pointer.as_u64());
        if index == 14 {
            let cr2 = x86_64::registers::control::Cr2::read().as_u64();
            w.write_string(" CR2=");
            write_hex(&mut w, cr2);
        }
        w.write_string("\n");
        crate::scheduler::user_fault_exit();
    }
    w.write_string("\n");
    halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _err: u64) -> ! {
    let mut w = crate::vga::Writer::new();
    w.write_string("DOUBLE FAULT\n");
    crate::vga::serial_write_atomic("[df] RIP=");
    serial_hex_u64(frame.instruction_pointer.as_u64());
    crate::vga::serial_write_atomic(" RSP=");
    serial_hex_u64(frame.stack_pointer.as_u64());
    crate::vga::serial_write_atomic("\n");
    halt_loop()
}

fn init_idt() {
    let idt = Box::leak(Box::new(InterruptDescriptorTable::new()));
    set_general_handler!(idt, exception_handler);
    unsafe {
        idt[TIMER_IRQ_VECTOR as usize].set_handler_addr(VirtAddr::new(crate::scheduler::timer_entry_addr()));
    }
    unsafe {
        idt[KEYBOARD_IRQ_VECTOR as usize]
            .set_handler_addr(VirtAddr::new(crate::keyboard::keyboard_irq_handler_addr()));
    }
    unsafe {
        idt[0x80usize]
            .set_handler_addr(VirtAddr::new(crate::syscall::int80_entry_addr()))
            .set_privilege_level(PrivilegeLevel::Ring3);
    }
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.load();
}

/// Ремап PIC на 0x20-0x2F и маскирование всех IRQ, кроме IRQ0 (таймер)
/// и IRQ1 (клавиатура).
fn init_pic() {
    unsafe {
        let mut cmd_m = Port::new(0x20);
        let mut data_m = Port::new(0x21);
        let mut cmd_s = Port::new(0xA0);
        let mut data_s = Port::new(0xA1);
        cmd_m.write(0x11u8); // ICW1: init, expect ICW4
        cmd_s.write(0x11u8);
        data_m.write(0x20u8); // ICW2: master base 0x20
        data_s.write(0x28u8); // ICW2: slave base 0x28
        data_m.write(0x04u8); // ICW3: slave on IRQ2
        data_s.write(0x02u8); // ICW3: slave cascade id 2
        data_m.write(0x01u8); // ICW4: 8086 mode
        data_s.write(0x01u8);
        data_m.write(0xFCu8); // mask: IRQ0 + IRQ1 enabled
        data_s.write(0xFFu8); // slave fully masked
    }
}

/// PIT, канал 0, ~100 Гц (делитель 11932).
fn init_pit() {
    unsafe {
        let mut cmd = Port::new(0x43);
        let mut data = Port::new(0x40);
        cmd.write(0x36u8); // ch0, lobyte/hibyte, mode 3, binary
        let divisor: u16 = 11932;
        data.write((divisor & 0xFF) as u8);
        data.write((divisor >> 8) as u8);
    }
}

pub fn init() {
    init_gdt();
    init_idt();
    init_pic();
    init_pit();
    x86_64::instructions::interrupts::enable();
}

pub fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

fn write_hex(w: &mut crate::vga::Writer, v: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [b'0'; 16];
    let mut n = v;
    let mut i = 16;
    loop {
        i -= 1;
        buf[i] = DIGITS[(n & 0xF) as usize];
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    write_bytes(w, &buf[i..]);
}

fn write_bytes(w: &mut crate::vga::Writer, b: &[u8]) {
    for &c in b {
        if c == 0 {
            break;
        }
        w.write_byte(c);
    }
}

/// Временная диагностика 3.2: hex u64 в serial.
fn serial_hex_u64(v: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        buf[2 + i] = HEX[((v >> (60 - i * 4)) & 0xf) as usize];
    }
    core::str::from_utf8(&buf).map(|s| crate::vga::serial_write_atomic(s));
}

/// Диагностика: байты (имя задачи) в serial, останавливаясь на NUL.
fn serial_write_bytes(b: &[u8]) {
    for &c in b {
        if c == 0 {
            break;
        }
        crate::vga::serial_putchar(c);
    }
}
