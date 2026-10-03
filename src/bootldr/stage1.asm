; src/bootldr/stage1.asm
;
; Первая стадия загрузчика: 512 байт в MBR установленной системы.
; Реальный режим, 16 бит. Задача минимальна - включить A20, положить
; stage2 в память и прыгнуть в неё. Всё остальное (карта памяти, ELF,
; переход в защищённый режим) делает stage2.
;
; Собирается nasm -f bin, размер обязан быть ровно 512 байт.

%include "layout.inc"

bits 16
org 0x7C00

_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00
    sti

    call init_cursor

    ; BIOS передаёт номер загрузочного привода в DL. Сохраняем немедленно:
    ; после переключений он уже невалиден.
    mov [BOOT_DRIVE_ADDR], dl

    call enable_a20

    ; --- читаем stage2 (STAGE2_SECTORS секторов с LBA 1) в 0000:8000 ---
    mov si, DAP_ADDR
    mov byte [si + 0], 0x10
    mov byte [si + 1], 0x00
    mov word [si + 2], STAGE2_SECTORS
    mov word [si + 4], STAGE2_LOAD_OFF
    mov word [si + 6], 0x0000
    mov dword [si + 8], STAGE2_LBA
    mov dword [si + 12], 0

    mov dl, [BOOT_DRIVE_ADDR]
    mov ah, 0x42
    int 0x13
    jc disk_error

    jmp 0000:STAGE2_LOAD_OFF

; Вывод текста подключается здесь, а не в начале файла: код из include
; оказался бы на смещении 0, а BIOS прыгает ровно на 0x7C00. Сюда
; управление никогда не проваливается - выше безусловный jmp.
%include "print.inc"

disk_error:
    mov si, err_msg
    call print_str
.hang:
    hlt
    jmp .hang

; Включение A20. Сначала быстрый гейт (порт 0x92): одна операция вместо
; десяти, и он есть на всём, что новее 286. Если бит уже выставлен - выходим,
; не трогая остальные биты порта (бит 0 там - системный сброс).
enable_a20:
    in al, 0x92
    test al, 0x02
    jnz .done
    and al, 0xFE                ; бит 0 не трогаем: это fast reset
    or al, 0x02
    out 0x92, al
.done:
    ret

err_msg: db "VITOS: boot: cannot read stage2", 13, 10, 0

; Остаток сектора: с 0x1F0 - номер привода, дальше до 0x1FE - заполнитель,
; в 0x1FE - сигнатура загрузки.
    times BOOT_DRIVE_OFF - ($ - $$) db 0
    db 0                        ; 0x7DF0: номер привода, пишет BIOS-овский DL
    times 510 - ($ - $$) db 0
    dw 0xAA55