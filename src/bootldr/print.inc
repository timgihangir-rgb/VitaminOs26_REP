; src/bootldr/print.inc
;
; Вывод текста для обеих стадий загрузчика. Включается и в stage1, и в
; stage2, поэтому не тянет за собой ничего лишнего.
;
; Печатаем сами, в COM1 и в текстовый буфер видеопамяти, а не через
; int 10h. Причина в том, что из 32-битного защищённого режима int 10h
; непригоден: CPU кладёт кадр прерывания в соответствии с D-битом SS, то
; есть 32-битный, а BIOS разбирает его как 16-битный. На QEMU это ещё
; может сойти, на настоящей машине - нет.
;
; Второе, не менее важное: COM1 даёт лог, который не мигает. Отладка
; загрузчика идёт по нему, и по нему же видно, что происходит, когда
; экран уже нельзя прочитать.

VGA_ROW: db 0
VGA_COL: db 0

; Спросить у BIOS текущее место курсора, чтобы продолжить строку там, где
; остановился загрузчик (BIOS уже напечатал свой баннер). Работает только
; в реальном режиме, поэтому вызывается до переключения.
init_cursor:
    push ax
    push bx
    push cx
    push dx
    mov ah, 0x03
    xor bh, bh
    int 0x10
    mov [VGA_ROW], dh
    mov [VGA_COL], dl
    pop dx
    pop cx
    pop bx
    pop ax
    ret

; Один символ: al. В COM1 и в видеопамять.
putc:
    push ax
    push cx
    push dx
    mov ah, al

    ; --- COM1. Ждём освобождения передатчика (LSR, бит 5 = THR empty) ---
    mov dx, 0x3FD
    in al, dx
    test al, 0x20
    jz .no_serial
    mov dx, 0x3F8
    mov al, ah
    out dx, al
.no_serial:

    cmp ah, 13
    je .carriage
    cmp ah, 10
    je .line_feed

    ; --- обычный символ в текущую ячейку ---
    movzx eax, ah
    or ax, 0x0700              ; символ + атрибут "светло-серый на чёрном"
    push eax
    movzx eax, byte [VGA_ROW]
    shl eax, 1
    shl eax, 3
    shl eax, 2                 ; *160: столько байт в строке 80x25
    movzx ecx, byte [VGA_COL]
    shl ecx, 1
    add eax, ecx
    pop ecx                    ; атрибут: у cx он и должен лежать
    mov [0xB8000 + eax], cx
    inc byte [VGA_COL]
    cmp byte [VGA_COL], 80
    jb .done
    mov byte [VGA_COL], 0
    jmp .next_line

.carriage:
    mov byte [VGA_COL], 0
    jmp .done

.line_feed:
.next_line:
    inc byte [VGA_ROW]
    cmp byte [VGA_ROW], 25
    jb .done
    mov byte [VGA_ROW], 0
    call scroll_up

.done:
    pop dx
    pop cx
    pop ax
    ret

; Прокрутка на строку: строки 1..24 вверх, последняя затирается пробелами.
; a32 обязателен: rep movsd/stosw адресуют через ESI/EDI, а по умолчанию
; в 16-битном режиме nasm берёт только SI/DI и адрес получается младшим.
scroll_up:
    push esi
    push edi
    push cx
    mov edi, 0xB8000
    mov esi, 0xB8200            ; на строку ниже
    mov ecx, (24 * 80 * 2) / 4  ; 3840 байт = 960 dword
    cld
    a32 rep movsd
    mov ecx, 80
    mov ax, 0x0720              ; пробелы с тем же атрибутом
    a32 rep stosw
    pop cx
    pop edi
    pop esi
    ret

; Строка, si - адрес, конец - ноль.
print_str:
    push ax
    push cx
    push dx
    push si
.next:
    lodsb
    test al, al
    jz .done
    call putc
    jmp .next
.done:
    pop si
    pop dx
    pop cx
    pop ax
    ret