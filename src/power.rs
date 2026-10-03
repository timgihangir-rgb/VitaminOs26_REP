//! src/power.rs
//!
//! Выключение и перезагрузка. Основной путь - ACPI (см. `src/acpi.rs`):
//! на настоящей материнской плате по-другому не выключишь. Дальше идут
//! способы, которые нужны для QEMU и для машин без ACPI, и в конце -
//! окончательная остановка процессора.
//!
//! Обе команды не возвращаются: вызывающий код сначала обязан сбросить ФС
//! на диск (`vitafs::sync_all()`), а потом уже звать сюда.

use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

use crate::vga::Writer;

/// Запись в порт ввода-вывода.
#[inline]
unsafe fn out8(port: u16, value: u8) {
    Port::new(port).write(value);
}

/// Чтение порта ввода-вывода.
#[inline]
unsafe fn in8(port: u16) -> u8 {
    Port::new(port).read()
}

/// Сколько ждём, чтобы последняя строка на экране и в COM1 гарантированно
/// успела появиться до того, как питание исчезнет. На экран пишем прямо в
/// видеопамять, так что там задержка не нужна - только на последовательный
/// порт, у которого есть своя буферизация.
fn settle() {
    for _ in 0..400_000 {
        core::hint::spin_loop();
    }
}

/// Замораживает систему перед тем как дёргать питание.
///
/// Прерывания выключаются, потому что после этого момента состояние системы
/// уже не имеет значения: либо машина уйдёт в выключение, либо мы останемся
/// стоять на месте. Пока прерывания включены, фоновые задачи продолжали бы
/// работать и могли бы начать запись в ФС после `sync_all()`.
fn freeze() {
    interrupts::disable();
}

/// Работаем ли мы под гипервизором.
///
/// Нужно, чтобы не трогать порты отладки (0xF4 и подобные) на настоящем
/// железе: за ними стоит реальное устройство, и запись может означать не то,
/// что мы думаем.
fn under_hypervisor() -> bool {
    // CPUID, лист 1: ECX, бит 31 - «процессор работает под гипервизором».
    let ecx = unsafe { core::arch::x86_64::__cpuid(1) }.ecx;
    ecx & (1 << 31) != 0
}

/// Выключение питания.
pub fn shutdown(writer: &mut Writer) -> ! {
    writer.write_string("Shutting down...\n");
    crate::vga::serial_write_atomic("[P] shutdown requested\n");
    settle();
    freeze();

    // 1. ACPI S5. На настоящем железе это единственное, что работает.
    //    На QEMU ACPI тоже есть, но путь ниже всё равно пригодится, если
    //    BIOS отдаёт кривую таблицу.
    if crate::acpi::power_off() {
        crate::vga::serial_write_atomic("[P] acpi S5 did not stop the machine\n");
    }

    // 2. Запасные порты QEMU. ACPI PM в QEMU висит на фиксированных адресах
    //    чипсета: 0x600 у ICH9 (q35) и 0xB000 у PIIX4 (i440fx).
    if under_hypervisor() {
        for port in [0x0604u16, 0x0B04] {
            unsafe {
                out8(port, 0x20);
                out8(port, 0x20);
            }
        }
        // 3. isa-debug-exit (-device isa-debug-exit,iobase=0xf4): QEMU на этом
        //    порту завершает работу. Удобно для автотестов.
        unsafe { out8(0x00F4, 0x10) };
    }

    // 4. Ничего не сработало. Прерывания уже выключены, так что hlt больше
    //    не разбудит процессор и система гарантированно замирает.
    writer.write_string("shutdown: no power-off path available, CPU halted\n");
    crate::vga::serial_write_atomic("[P] halted\n");
    halt();
}

/// Перезагрузка.
pub fn reboot(writer: &mut Writer) -> ! {
    writer.write_string("Rebooting...\n");
    crate::vga::serial_write_atomic("[P] reboot requested\n");
    settle();
    freeze();

    // 1. Регистр сброса из FADT - так умеет всё железо новее примерно 2004 года.
    if crate::acpi::reset() {
        crate::vga::serial_write_atomic("[P] acpi reset did not reboot\n");
    }

    // 2. Клавиатурный контроллер 8042: импульс сброса. Работает на всём,
    //    включая машины без ACPI, поэтому идёт до чипсетных трюков.
    keyboard_controller_reset();

    // 3. Порт 0xCF9 - сброс через чипсет Intel. 0x06 - обычный сброс,
    //    0x0E - сброс с очисткой CMOS. Нужен именно этот порядок и именно
    //    эти два значения: процессор ждёт второй записи.
    unsafe {
        out8(0x0CF9, 0x06);
        out8(0x0CF9, 0x0E);
    }

    // 4. isa-debug-exit под гипервизором.
    if under_hypervisor() {
        unsafe { out8(0x00F4, 0x10) };
    }

    writer.write_string("reboot: no reset path available, CPU halted\n");
    crate::vga::serial_write_atomic("[P] reboot halted\n");
    halt();
}

/// Сброс через 8042. Контроллер нужно дождаться перед каждой командой,
/// иначе на части машин импульс теряется.
fn keyboard_controller_reset() {
    /// Ждём, пока контроллер освободит входной буфер (бит 1 регистра 0x64).
    fn wait_input_clear() {
        for _ in 0..1_000_000 {
            if unsafe { in8(0x64) } & 0x02 == 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Ждём, пока контроллер выдаст ответ (бит 0 регистра 0x64).
    fn wait_output_full() {
        for _ in 0..1_000_000 {
            if unsafe { in8(0x64) } & 0x01 != 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    unsafe {
        wait_input_clear();
        // 0xFE в порт 0x64 - сброс процессора.
        out8(0x64, 0xFE);
        wait_input_clear();
        wait_output_full();
    }
}

/// Окончательная остановка: прерывания выключены, hlt не будит процессор.
fn halt() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}
