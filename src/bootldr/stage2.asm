; src/bootldr/stage2.asm
;
; Вторая стадия загрузчика установленной системы. Грузится stage1'ом в
; 0000:8000, начинает в реальном режиме, заканчивает в защищённом
; 32-битном.
;
; Что делает:
;   1. включает A20 (stage1 уже включал - подтверждаем);
;   2. снимает карту памяти через INT 15h AX=E820;
;   3. переходит в защищённый режим;
;   4. собирает multiboot2-инфо (тег 6 - карта, тег 4 - базовая информация,
;      end-тег);
;   5. разбирает ELF64 и кладёт каждый PT_LOAD по p_paddr, обнуляя .bss
;      (p_memsz > p_filesz);
;   6. прыгает во _start ядра с EAX = magic, EBX = указатель на инфо.
;
; Чтение диска идёт через INT 13h, а он существует только в реальном
; режиме. Поэтому disk_to_phys, работая из pmode, зовёт bios_read - тот
; переключается в реальный режим, делает INT 13h и возвращается. Буфер
; чтения (BUF_ADDR) обязан быть ниже 1 МиБ: адрес в DAP - сегмент:смещение.
;
; Формат инфо - ровно тот, что уже разбирает src/memory.rs: тег 6 с
; entry_size = 24. _start - 32-битная точка входа (src/boot.asm, bits 32),
; поэтому прыгаем из 32-битного защищённого режима.

%include "layout.inc"

bits 16
org 0x8000

MB_MAGIC        equ 0xE85250D6

stage2_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00
    sti

    call init_cursor
    call enable_a20

    mov si, msg_start
    call print_str

    call collect_e820

    ; --- переход в защищённый 32-битный режим ---
    cli
    lgdt [pm_gdt_ptr]
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov esp, STACK_TOP
    mov eax, cr0
    or eax, 0x1
    mov cr0, eax
    jmp dword 0x08:pm32_entry

; ---------------------------------------------------------------------------
; Реальный режим: A20
; ---------------------------------------------------------------------------
enable_a20:
    in al, 0x92
    test al, 0x02
    jnz .done
    call empty_8042
    mov al, 0xAD                ; отключить клавиатуру
    out 0x64, al
    call empty_8042
    mov al, 0xD0                ; прочитать выходной регистр
    out 0x64, al
    call empty_8042
    in al, 0x60
    or al, 0x02                 ; включить A20
    out 0x64, al
    call empty_8042
    mov al, 0xAE                ; вернуть клавиатуру
    out 0x64, al
.done:
    ret

empty_8042:
    in al, 0x64
    test al, 0x02
    jnz empty_8042
    ret

; ---------------------------------------------------------------------------
; Реальный режим: карта памяти E820
;
; Кладём сырые 20-байтные записи в E820_BUF. Количество - E820_COUNT.
; Если E820 не ответил (CF или не 'SMAP'), подставляем одну правдоподобную
; запись 1 МиБ..2 ГиБ: пустая карта означает, что ядро не найдёт ни
; кадра, ни кучи, и остановится на сообщении о нехватке памяти.
; ---------------------------------------------------------------------------
collect_e820:
    mov word [E820_COUNT], 0
    mov di, E820_BUF
    xor ebx, ebx                ; продолжение списка; 0 - конец
.next:
    mov eax, 0xE820
    mov edx, 0x534D5344         ; 'SMAP'
    mov ecx, 20                 ; BIOS пишет ровно 20 байт
    int 0x15
    jc .done
    cmp eax, 0x534D5344
    jne .done
    add di, 20
    inc word [E820_COUNT]
    cmp word [E820_COUNT], E820_MAX
    jae .done
    test ebx, ebx
    jnz .next
.done:
    cmp word [E820_COUNT], 0
    jne .have
    ; аварийная карта: 1 МиБ .. 2 ГиБ
    mov di, E820_BUF
    mov dword [di + 0], 0x00100000
    mov dword [di + 4], 0x00000000
    mov dword [di + 8], 0x7FF00000
    mov dword [di + 12], 0x00000000
    mov dword [di + 16], 1
    mov word [E820_COUNT], 1
.have:
    ret

; Вывод текста подключается здесь, а не в начале файла: код из include
; оказался бы на смещении 0, а stage1 прыгает ровно на 0x8000.
%include "print.inc"

; ---------------------------------------------------------------------------
; Защищённый режим, 32 бита
; ---------------------------------------------------------------------------
bits 32
pm32_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov esp, STACK_TOP

    call calc_meminfo
    call build_mb_info
    mov esi, msg_mb
    call print_str32
    call load_elf
    mov esi, msg_elf
    call print_str32
    mov esi, msg_jump
    call print_str32

    ; _start ожидает EAX = magic, EBX = физический адрес multiboot2-инфо.
    mov ebx, MB_INFO
    mov eax, MB_MAGIC
    jmp [E_ENTRY]

; ---------------------------------------------------------------------------
; Тег 3 (модули) не строим - у ядра их нет. Тег 6 - карта, тег 4 - базовая
; информация о памяти.
; ---------------------------------------------------------------------------
build_mb_info:
    mov edi, MB_INFO
    mov dword [edi + 0], 0         ; total_size, заполним в конце
    mov dword [edi + 4], 0
    add edi, 8

    ; ---- тег 6: карта памяти, шаг записи 24 байта ----
    mov dword [edi + 0], 6
    mov dword [edi + 8], 24
    mov dword [edi + 12], 0
    mov esi, E820_BUF
    movzx ecx, word [E820_COUNT]
    mov ebp, 16                    ; смещение записи внутри тега
.copy6:
    test ecx, ecx
    jz .copy6_done
    push ecx
    push esi
    push edi
    add edi, ebp
    mov ecx, 5                     ; 20 байт = 5 dword
    cld
    rep movsd
    mov dword [edi], 0             ; reserved
    pop edi
    pop esi
    add esi, 20
    add ebp, 24
    pop ecx
    dec ecx
    jmp .copy6
.copy6_done:
    mov [edi + 4], ebp             ; размер тега = 16 + n*24

    ; ---- тег 4: базовая информация о памяти ----
    add edi, ebp
    mov dword [edi + 0], 4
    mov dword [edi + 4], 16
    mov eax, [MEM_LOWER_KB]
    mov [edi + 8], eax
    mov eax, [MEM_UPPER_KB]
    mov [edi + 12], eax
    add edi, 16

    ; ---- end-тег ----
    mov dword [edi + 0], 0
    mov dword [edi + 4], 8
    add edi, 8

    sub edi, MB_INFO
    mov [MB_INFO + 0], edi
    ret

; ---------------------------------------------------------------------------
; Базовая информация о памяти (тег 4). Считается по сырой E820: сумма
; доступных областей ниже и выше 1 МиБ в КиБ. Для этого тега 64-битную
; длину можно обрезать до 32 бит - он информационный, точная карта приходит
; тегом 6.
; ---------------------------------------------------------------------------
calc_meminfo:
    mov dword [MEM_UPPER_KB], 0
    mov dword [MEM_LOWER_KB], 0
    mov esi, E820_BUF
    movzx ecx, word [E820_COUNT]
.loop:
    test ecx, ecx
    jz .done
    cmp dword [esi + 16], 1        ; только type=1 (доступно)
    jne .next
    mov eax, [esi]                 ; base
    mov edx, [esi + 8]             ; len
    add eax, edx                   ; конец области (32 бита)
    cmp eax, 0x100000
    jbe .all_low
    ; область выше 1 МиБ; если начиналась ниже - делим
    cmp dword [esi], 0x100000
    jae .upper_only
    mov ebx, 0x100000
    sub ebx, [esi]
    shr ebx, 10
    add [MEM_LOWER_KB], ebx
    jmp .upper_only
.all_low:
    shr eax, 10
    add [MEM_LOWER_KB], eax
    jmp .next
.upper_only:
    sub eax, 0x100000
    shr eax, 10
    add [MEM_UPPER_KB], eax
.next:
    add esi, 20
    dec ecx
    jmp .loop
.done:
    ret

; ---------------------------------------------------------------------------
; Загрузка ELF64 и прыжок в ядро
;
; Таблицу программных заголовков копируем из буфера в PHDR_BUF до начала
; загрузки сегментов: первая же запись в буфер уничтожила бы исходные
; заголовки, а они нужны для всех PT_LOAD подряд.
; Индекс держим в памяти по той же причине - load_segment затирает ebx.
; ---------------------------------------------------------------------------
load_elf:
    ; первые 4 КиБ образа - заголовок и таблица программных заголовков
    mov ebx, KERNEL_LBA
    mov edx, 8
    mov esi, BUF_ADDR
    call read_disk_at

    cmp dword [BUF_ADDR + 0], 0x464C457F     ; 7F 'E' 'L' 'F'
    jne elf_error
    cmp byte [BUF_ADDR + 4], 2                ; ELFCLASS64
    jne elf_error
    cmp byte [BUF_ADDR + 5], 1                ; little endian
    jne elf_error
    cmp word [BUF_ADDR + 16], 2               ; ET_EXEC
    jne elf_error

    mov eax, [BUF_ADDR + 0x18]                ; e_entry
    mov [E_ENTRY], eax
    mov eax, [BUF_ADDR + 0x20]                ; e_phoff
    mov [E_PHOFF], eax
    movzx eax, word [BUF_ADDR + 0x36]         ; e_phentsize
    mov [E_PHENT], eax
    movzx eax, word [BUF_ADDR + 0x38]         ; e_phnum
    mov [E_PHNUM], eax

    ; размер таблицы заголовков с округлением до dword: копируем rep movsd
    mov eax, [E_PHNUM]
    test eax, eax
    jz elf_error
    imul eax, [E_PHENT]
    cmp eax, PHDR_BYTES
    ja elf_error                              ; не влезает - загрузчик не виноват
    add eax, 3
    and eax, ~3
    mov [PHDR_LEN], eax
    shr eax, 2
    mov ecx, eax
    mov esi, BUF_ADDR
    add esi, [E_PHOFF]
    mov edi, PHDR_BUF
    cld
    rep movsd

    mov dword [PH_IDX], 0
.ph_loop:
    mov ebx, [PH_IDX]
    cmp ebx, [E_PHNUM]
    jae .done
    mov eax, ebx
    imul eax, [E_PHENT]
    add eax, PHDR_BUF
    mov esi, eax
    cmp dword [esi + 0], 1                    ; PT_LOAD
    jne .next
    mov eax, [esi + 8]                        ; p_offset
    mov [P_OFFSET], eax
    mov eax, [esi + 24]                       ; p_paddr
    mov [P_PADDR], eax
    mov eax, [esi + 32]                       ; p_filesz
    mov [P_FILESZ], eax
    mov eax, [esi + 40]                       ; p_memsz
    mov [P_MEMSZ], eax
    call load_segment
.next:
    inc dword [PH_IDX]
    jmp .ph_loop
.done:
    ret

elf_error:
    mov esi, err_elf
    call print_str32
.hang:
    hlt
    jmp .hang

; Один PT_LOAD: положить p_filesz байт по p_paddr, занулить хвост до p_memsz.
;
; Читаем с округлением вверх до сектора. Если p_offset не выровнен по 512,
; лишние байты в начале сектора попадут по p_paddr - наш линкер даёт
; выравнивание 0x1000, так что этого не бывает, но проверка всё равно
; нужна: молча сдвинутый сегмент хуже явной ошибки.
load_segment:
    mov eax, [P_OFFSET]
    test eax, 0x1FF
    jnz elf_error                ; p_offset не выровнен по границе сектора
    shr eax, 9
    add eax, KERNEL_LBA
    mov ebx, eax
    mov eax, [P_FILESZ]
    add eax, 511
    shr eax, 9
    test eax, eax
    jz .no_zero                  ; пустой сегмент (bss-only) - грузить нечего
    mov ecx, eax
    mov edi, [P_PADDR]
    call disk_to_phys
.no_zero:

    mov eax, [P_MEMSZ]
    cmp eax, [P_FILESZ]
    jbe .done
    sub eax, [P_FILESZ]
    add eax, 3
    shr eax, 2
    mov edi, [P_PADDR]
    add edi, [P_FILESZ]
    xor eax, eax
    cld
    rep stosd
.done:
    ret

; ---------------------------------------------------------------------------
; Диск -> физическая память
;   вход: ebx = LBA, ecx = число секторов, edi = физический адрес
;
; Счётчик оставшихся секторов живёт в памяти, а не в регистре: ниже он же
; превращается в счётчик байт для rep movsd, а размер порции (edx)
; переживает вызов чтения только в стеке - bios_read затирает регистры.
; ---------------------------------------------------------------------------
disk_to_phys:
    mov [DST], edi
    mov [LEFT], ecx
.chunk:
    mov ecx, [LEFT]
    test ecx, ecx
    jz .done
    mov edx, ecx
    cmp edx, BUF_SECTORS
    jbe .size_ok
    mov edx, BUF_SECTORS
.size_ok:
    push edx                 ; размер порции переживает вызов чтения
    mov esi, BUF_ADDR        ; DAP адресует меньше 1 МиБ - читаем в буфер
    call read_disk_at
    pop edx
    ; копирование: edx секторов из буфера на место
    mov esi, BUF_ADDR
    mov edi, [DST]
    mov ecx, edx
    shl ecx, 7               ; секторов * 128 dword = * 512 байт
    cld
    rep movsd
    ; продвигаем LBA, адрес назначения и остаток
    mov eax, edx
    add ebx, eax
    shl eax, 9               ; секторов * 512 байт
    add [DST], eax
    sub [LEFT], edx
    jmp .chunk
.done:
    ret

; ---------------------------------------------------------------------------
; Чтение секторов с привода в низкую память
;   вход: ebx = LBA, edx = число секторов, esi = линейный адрес (< 1 МиБ)
; ---------------------------------------------------------------------------
read_disk_at:
    push ebx
    push edx
    push esi
    push edi
    mov byte [DAP_ADDR + 0], 0x10
    mov byte [DAP_ADDR + 1], 0
    mov [DAP_ADDR + 2], dx
    mov eax, esi
    and eax, 0xF
    mov [DAP_ADDR + 4], ax
    mov eax, esi
    shr eax, 4
    mov [DAP_ADDR + 6], ax
    mov [DAP_ADDR + 8], ebx
    mov dword [DAP_ADDR + 12], 0
    call bios_read
    test byte [BIOS_CF], 1
    jnz .failed
    pop edi
    pop esi
    pop edx
    pop ebx
    ret
.failed:
    mov esi, err_read
    call print_str32
.hang:
    hlt
    jmp .hang

; ---------------------------------------------------------------------------
; Переход pmode -> реальный режим -> INT 13h -> pmode.
;
; Сначала прыгаем на 16-битный сегмент GDT (0x18), уже оттуда снимаем PE:
; если снять PE раньше, far jump загрузит CS по правилам реального режима
; (base = sel*16), а не из GDT. Обратно - стандартно: включаем PE и far
; jump на плоский 32-битный сегмент 0x08.
; ---------------------------------------------------------------------------
bios_read:
    mov [SAVED_ESP], esp
    jmp 0x18:rm_bios

pm_after_bios:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov esp, [SAVED_ESP]
    ret

%include "print32.inc"

; ---------------------------------------------------------------------------
; 16-битный трамплин BIOS и GDT/данные
; ---------------------------------------------------------------------------
bits 16
rm_bios:
    mov ax, 0x20
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x7C00
    mov eax, cr0
    and eax, ~1
    mov cr0, eax
    ; CS всё ещё несёт селектор 0x18. В реальном режиме far jump
    ; трактует селектор как base = sel*16, поэтому IRET из BIOS вернул бы
    ; управление на 0x180+offset. Перезагружаем CS нулём.
    jmp 0:rm_real
rm_real:
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7C00
    sti
    mov dl, [BOOT_DRIVE_ADDR]
    mov ah, 0x42
    mov si, DAP_ADDR
    int 0x13
    setc [BIOS_CF]
    cli
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 0x08:pm_after_bios

; GDT: 0x08 - плоский 32-битный код, 0x10 - плоские данные,
; 0x18 - 16-битный код (для возврата в реальный режим),
; 0x20 - 16-битные данные.
pm_gdt_data:
    dq 0x0000000000000000
    dq 0x00CF9A000000FFFF        ; 0x08
    dq 0x00CF92000000FFFF        ; 0x10
    dq 0x00009A000000FFFF        ; 0x18
    dq 0x000092000000FFFF        ; 0x20
pm_gdt_end:

pm_gdt_ptr:
    dw pm_gdt_end - pm_gdt_data - 1
    dd pm_gdt_data

; ---------------------------------------------------------------------------
; Сообщения и переменные
; ---------------------------------------------------------------------------
msg_start:  db "VITOS: stage2", 13, 10, 0
msg_mb:     db "VITOS: mb-info done", 13, 10, 0
msg_elf:    db "VITOS: elf loaded", 13, 10, 0
msg_jump:   db "VITOS: jumping to kernel", 13, 10, 0
err_read:   db "VITOS: disk read failed", 13, 10, 0
err_elf:    db "VITOS: bad kernel ELF", 13, 10, 0

E820_COUNT:  dw 0
DST:        dd 0
LEFT:       dd 0                ; секторов осталось в disk_to_phys
PH_IDX:     dd 0                ; индекс phdr (load_segment затирает ebx)
PHDR_LEN:   dd 0
MEM_LOWER_KB: dd 0
MEM_UPPER_KB: dd 0
E_ENTRY:    dd 0
E_PHOFF:    dd 0
E_PHENT:    dd 0
E_PHNUM:    dd 0
P_OFFSET:   dd 0
P_PADDR:    dd 0
P_FILESZ:   dd 0
P_MEMSZ:    dd 0
