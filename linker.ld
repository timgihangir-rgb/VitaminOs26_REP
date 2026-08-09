ENTRY(_start)

/* Boot trampoline runs identity-mapped at 2 MiB; the kernel itself is linked
 * at the high-half VMA (0xFFFF800000000000 + physical offset) so that every
 * process PML4 can share the same kernel mapping. Each kernel section is
 * loaded (AT/LMA) at a physical address = VMA - HIGH_HALF. */

HIGH_HALF = 0xFFFF800000000000;

PHDRS
{
    boot   PT_LOAD FLAGS(5); /* R+X: multiboot header + 32-bit trampoline */
    bootrw PT_LOAD FLAGS(6); /* R+W: page tables, GDT, boot stack */
    text   PT_LOAD FLAGS(5); /* R+X: kernel .rodata/.text */
    data   PT_LOAD FLAGS(6); /* R+W: kernel .data/.bss */
}

SECTIONS
{
    /* ─── Boot trampoline: identity-mapped, low VMA = LMA ─── */
    . = 2M;

    _boot_start = .;

    .multiboot_header : {
        KEEP(*(.multiboot_header))
    } :boot

    .boot.text : {
        *(.boot.text)
    } :boot

    . = ALIGN(4K);

    .boot.data : {
        *(.boot.data)
    } :bootrw

    .boot.bss (NOLOAD) : {
        *(.boot.bss)
    } :bootrw

    . = ALIGN(2M);
    _boot_end = .;
    _kernel_phys = .;

    /* ─── Kernel: high-half VMA, physical LMA ─── */
    . = HIGH_HALF + _kernel_phys;
    _kernel_start = .;

    _lma = _kernel_phys;

    .rodata : AT(_lma) {
        *(.rodata)
        *(.rodata.*)
    } :text
    . = ALIGN(4K);
    _lma = ALIGN(_lma + SIZEOF(.rodata), 4K);

    .text : AT(_lma) {
        *(.text)
        *(.text.*)
    } :text
    . = ALIGN(4K);
    _lma = ALIGN(_lma + SIZEOF(.text), 4K);

    .data : AT(_lma) {
        *(.data)
        *(.data.*)
        *(.data.rel.ro)
        *(.data.rel.ro.*)
        *(.got)
        *(.got.*)
        *(.init_array)
        *(.fini_array)
    } :data
    . = ALIGN(4K);
    _lma = ALIGN(_lma + SIZEOF(.data), 4K);

    .bss (NOLOAD) : AT(_lma) {
        . = ALIGN(8);
        *(.bss)
        *(.bss.*)
        *(COMMON)
    } :data
    . = ALIGN(4K);
    _lma = ALIGN(_lma + SIZEOF(.bss), 4K);

    _kernel_end = .;
    _kernel_size = _kernel_end - _kernel_start;
    _kernel_lma_size = _lma - _kernel_phys;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.*)
    }
}
