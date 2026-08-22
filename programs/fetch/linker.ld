ENTRY(_start)

SECTIONS
{
    /* Ядро грузит ELF-сегменты по их p_vaddr; user-область начинается с 4 MiB. */
    . = 0x400000;
    .text : {
        KEEP(*(.text._start))
        *(.text)
        *(.text.*)
    }
    . = ALIGN(8);
    .rodata : {
        *(.rodata)
        *(.rodata.*)
    }
    . = ALIGN(8);
    .data : {
        *(.data)
        *(.data.*)
        *(.got)
        *(.got.*)
    }
    . = ALIGN(8);
    .bss : {
        *(.bss)
        *(.bss.*)
        *(COMMON)
    }
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.*)
        *(.note.gnu.property)
        *(.gcc_except_table)
    }
}
