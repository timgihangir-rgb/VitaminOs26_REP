; src/bootldr/print32.inc
;
; Вывод текста из защищённого 32-битного режима: COM1 + текстовый буфер
; видеопамяти напрямую. Намеренно дублирует print.inc: тот собран как
; 16-битный код и из pmode непригоден - у него другая разрядность
; операндов и адресов. Пользуется теми же переменными VGA_ROW/VGA_COL,
; которые определены в print.inc (включается в 16-битной части stage2).
;
; Имена с суффиксом 32, чтобы не конфликтовать с print.inc.

; Один символ: al.
putc32:
    push eax
    push ecx
    push edx
    mov ah, al

    ; --- COM1, ждём THR empty ---
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

    ; --- обычный символ ---
    movzx eax, ah
    or ax, 0x0700
    push eax
    movzx eax, byte [VGA_ROW]
    imul eax, 80
    movzx ecx, byte [VGA_COL]
    add eax, ecx
    shl eax, 1
    pop ecx
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
    call scroll_up32

.done:
    pop edx
    pop ecx
    pop eax
    ret

scroll_up32:
    push esi
    push edi
    push ecx
    mov edi, 0xB8000
    mov esi, 0xB8200
    mov ecx, (24 * 80 * 2) / 4
    cld
    rep movsd
    mov ecx, 80
    mov ax, 0x0720
    rep stosw
    pop ecx
    pop edi
    pop esi
    ret

; Строка, esi - адрес, конец - ноль.
print_str32:
    push eax
    push esi
.next:
    lodsb
    test al, al
    jz .done
    call putc32
    jmp .next
.done:
    pop esi
    pop eax
    ret
