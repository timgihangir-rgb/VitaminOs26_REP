ENTRY(_start)

SECTIONS
{
    . = 0;
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
