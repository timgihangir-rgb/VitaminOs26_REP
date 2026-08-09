# VitaminOS26 — что реализовано

Хобби-ОС на Rust (ядро) + x86_64 assembly (загрузка). Версия **v0.1.0**.

## Загрузка и старт
- Multiboot2 + GRUB, загрузка ELF по адресу 2 МБ.
- Собственный 32→64-битный трамплин в `src/boot.asm`: PML4/PDP/PD, PAE, long mode, GDT, стек 16 КБ, identity-маппинг первых 4 МБ.
- Баннер «VitaminOS26 - Welcome!», инициализация: VGA, COM1, память, файловая система, планировщик, PIT 100 Гц, init-система, шелл.

## Память
- Куча 1 МиБ (`linked_list_allocator`), офсет 4 ГБ, `OffsetPageTable`, frame-аллокатор из Multiboot2 memory map.
- Резерв низкой памяти под ABI: `0x5000` SysInfo, `0x6000..` буфер vita/help, `0x708C/0x7090` позиция выхода, `0x7800..` ABI-указатели.

## CPU / прерывания
- Один CPU, два PIC 8259 (IRQ0 = PIT 100 Гц, остальные замаскированы).
- GDT + TSS (IST для double fault), IDT, общий обработчик исключений, PIT 11932.
- CPUID: vendor, brand, family/model/stepping, флаги SSE/SSE2/AVX/RDRAND, cores/threads.

## Планировщик и задачи
- Вытесняющий round-robin 100 Гц, до 16 задач (0 = шелл), стек 16 КБ на задачу.
- `spawn / kill / list / ps`, состояния Ready/Running/Finished, переключение контекста на таймере.
- Фоновые демоны `bg`: `prime`, `fib`, `parallel` (воркеры с барьером через атомики), `ticker`, `crashy` (демо рестарта).

## Ввод / вывод
- PS/2-клавиатура опросом (scancode set 1): Shift, Backspace, стрелки, история команд.
- VGA-текст 80×30, BIOS-шрифт 8×16 (plane 2), курсор, скроллинг, цвета.
- Serial COM1 (лог для QEMU).

## Шелл (VitaminShell 0.1.0)
- Промпт `vitamin_os26:<pwd>$`.
- Команды: `clear`, `version`, `ls`, `pwd`, `cd`, `mkdir`, `rmdir`, `touch`, `rm`, `cat`, `cp`, `mv`, `find`, `echo` (в т.ч. `> file`), `run`, `bg`, `ps`, `kill`, `init`.
- Неизвестная команда → попытка запуска `/bin/<имя>` в foreground.

## Файловая система и диск
- In-memory VFS (дерево, арена узлов): `/bin`, `/etc`, `/home`, `/tmp`, `/var`, `/var/log`.
- ATA PIO (primary master), сериализованный слепок VFS на диске (magic `VOS26IMG`, суперблок сектор 0), персистентность между перезагрузками.
- Флаш на диск после каждой команды; загрузка слепка при старте.

## Пользовательские программы (`/bin`, внедрены в ядро)
- **clock** — CMOS RTC, 12/24 ч, конфиг `/etc/timezone`, демон-режим в `/tmp/clock.log`.
- **example** — «Hello from C!».
- **fetch** — ASCII-лого «V в круге» + системная информация (OS, kernel, CPU, память, флаги), продолжение tty при нехватке места.
- **help** — справка по командам и программам.
- **snake** — змейка 60×20 (WASD, Q), счёт на экране, тайминг по PIT.
- **vita** — текстовый редактор: до 500 строк/4 КБ, навигация, Ctrl+S, Ctrl+Q, аппаратный курсор.

## Исполнение программ
- Плоские бинарники (C / Rust no_std), встроены в ядро и разложены в `/bin`.
- Запуск в ring 0 в адресном пространстве ядра; SysInfo в `0x5000`; shared memory для vita/help; ABI: `ticks / hlt / vfs_read / vfs_write` (`0x7800..`).
- Программа пишет `EXIT_ROW/EXIT_COL` — шелл продолжает с этой позиции.

## Сборка и тесты
- Тулчейн `nightly-2022-11-01`, `build-std`, NASM (boot.asm), GCC (C-программы), GRUB → ISO + 8-МиБ диск.
- Запуск: `qemu-system-x86_64 -cdrom target/os.iso -drive file=target/os.img,format=raw,if=ide`.
- 7 автотестов (QEMU + QMP): команды, VFS, multitask, parallel, persist, fetch, init, vita.
