; src/boot.asm — Multiboot2 header and 32→64-bit trampoline
; Assembled with: nasm -f elf64 src/boot.asm -o $OUT/boot.o
;
; The kernel is linked at the high-half VMA (0xFFFF800000000000 + phys) but
; loaded at physical ~4 MiB by GRUB. This trampoline stays identity-mapped at
; 2 MiB, builds the page tables (identity 4 MiB + high-half window of the
; first 1 GiB of physical memory), switches to long mode and jumps into the
; high-half kernel via an absolute indirect call.

global _start
extern kernel_main

; ─── Multiboot2 header ───────────────────────────────────────────────────────
section .multiboot_header progbits alloc noexec nowrite align=8

mb_header_start:
    dd 0xE85250D6                       ; magic
    dd 0                                ; architecture (i386)
    dd mb_header_end - mb_header_start  ; header length
    dd 0x100000000 - (0xE85250D6 + 0 + (mb_header_end - mb_header_start)) ; checksum

    ; Information request tag: request memory map + basic meminfo
    align 8
    dw 1                                ; type: information request
    dw 0                                ; flags
    dd info_req_end - info_req_start    ; size
info_req_start:
    dd 4                                ; request basic memory info
    dd 6                                ; request memory map
info_req_end:

    ; End tag
    align 8
    dw 0                                ; type: end
    dw 0                                ; flags
    dd 8                                ; size
mb_header_end:

; ─── 32-bit entry point ──────────────────────────────────────────────────────
section .boot.text progbits alloc exec nowrite

bits 32
_start:
    mov [saved_mb_info], ebx
    mov [saved_magic], eax

    ; ── Page tables ──────────────────────────────────────────────────────────
    ; pml4[0]   = identity map of the first 4 MiB (boot code, low-memory ABI
    ;             structures, VGA buffer, multiboot info).
    mov eax, pdp0_table
    or  eax, 0x3
    mov [pml4_table], eax

    ; pml4[256] = high-half window at 0xFFFF800000000000 (first 1 GiB phys).
    mov eax, pdp_high_table
    or  eax, 0x3
    mov [pml4_table + 256*8], eax

    mov eax, pd0_table
    or  eax, 0x3
    mov [pdp0_table], eax

    mov edi, pd0_table
    mov ecx, 2
    mov eax, 0x83
.fill_pd:
    mov [edi], eax
    add eax, 0x200000
    add edi, 8
    dec ecx
    jnz .fill_pd

    ; Map phys 0..1 GiB into the high-half window as 2 MiB huge pages.
    mov edi, pdp_high_table
    xor eax, eax
    mov ecx, 512
.fill_high:
    mov [edi], eax
    or  dword [edi], 0x83
    add eax, 0x200000
    add edi, 8
    dec ecx
    jnz .fill_high

    ; ── Long-mode setup ──────────────────────────────────────────────────────
    mov eax, pml4_table
    mov cr3, eax

    mov eax, cr4
    or  eax, 1 << 5
    mov cr4, eax

    mov ecx, 0xC0000080
    rdmsr
    or  eax, 1 << 8
    wrmsr

    mov eax, cr0
    or  eax, 1 << 31
    mov cr0, eax

    lgdt [gdt_ptr]
    push dword gdt.code64 - gdt_start
    push dword _start64
    retf

; ─── 64-bit code ─────────────────────────────────────────────────────────────
bits 64
_start64:
    mov ax, gdt.data - gdt_start
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    mov rsp, stack_top

    mov rdi, [saved_magic]
    mov rsi, [saved_mb_info]
    and rsp, ~15

    ; The kernel lives at 0xFFFF800000000000+ — jump there with an absolute
    ; indirect call (a rel32 direct call would overflow from this low VMA).
    movabs rax, kernel_main
    call rax

.hang:
    cli
    hlt
    jmp .hang

; ─── Page tables (4 KiB-aligned) ──────────────────────────────────────────────
section .boot.data progbits alloc write noexec align=4096

pml4_table:       times 512 dq 0
pdp0_table:       times 512 dq 0
pd0_table:        times 512 dq 0
pdp_high_table:   times 512 dq 0

; ─── Global Descriptor Table ─────────────────────────────────────────────────
align 16
gdt_start:
    dq 0
gdt.code32:
    dw 0xFFFF, 0
    db 0, 0x9A, 0xCF, 0
gdt.code64:
    dw 0, 0
    db 0, 0x9A, 0x20, 0
gdt.data:
    dw 0xFFFF, 0
    db 0, 0x92, 0xCF, 0
gdt_end:

align 8
gdt_ptr:
    dw gdt_end - gdt_start - 1
    dq gdt_start

; ─── Saved boot info ─────────────────────────────────────────────────────────
align 8
saved_mb_info:  dq 0
saved_magic:    dq 0

; ─── Stack ───────────────────────────────────────────────────────────────────
section .boot.bss nobits alloc write noexec align=16

stack_bottom:
    ; 64 КиБ: слои ФС вкладывают кадры с 4КБ-буферами (dir->file->bcache).
    resb 65536
stack_top:
