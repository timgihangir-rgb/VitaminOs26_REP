#!/bin/bash
# scripts/make-installer-disk.sh
#
# Собирает загрузочный диск установщика VitaminOS26: тот, с которого машина
# грузится, чтобы поставить систему на другой диск.
#
#   scripts/make-installer-disk.sh --image target/installer-disk.img
#   scripts/make-installer-disk.sh --dd /dev/sdb --image target/installer-disk.img
#   scripts/make-installer-disk.sh /dev/sdb
#
# Раскладка повторяет src/bootldr/layout.inc (числа берутся оттуда, а не
# дублируются здесь):
#
#   LBA 0            stage1 (512 Б) + таблица разделов + 0x55AA
#   LBA 1..16        stage2
#   LBA 256..        сырой ELF ядра
#   LBA 32768..      раздел 1: VITAFS с маркером /etc/installer
#
# Загрузка идёт собственным загрузчиком ОС, GRUB не нужен. Так устроен и
# установочный ISO - он отдаёт то же ядро через GRUB, но ФС установщика
# лежит на разделе диска, а не на ISO: ядро не умеет читать ни USB mass
# storage, ни ATAPI, поэтому образ ФС обязан быть на диске, который виден
# блочному слою (AHCI/ATA).
#
# Требуются: nasm, python3, coreutils. root нужен только для записи на
# физическое устройство.
set -e

PROG="$(basename "$0")"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

LAYOUT="src/bootldr/layout.inc"
BOOTLDR="src/bootldr"
MBR_MAKER="scripts/mk-installer-mbr.py"
KERNEL="target/x86_64-unknown-none/debug/vitamin_os26"
FS_IMAGE="target/installer.img"
OUT_IMAGE=""
IMG_SIZE=""
DEVICE=""
DO_DD=0
ASSUME_YES=0
SECTOR=512

die()  { echo "ОШИБКА: $*" >&2; exit 1; }
info() { echo "  $*"; }

cleanup() { [ -n "${TMP:-}" ] && rm -rf "$TMP"; return 0; }
trap cleanup EXIT

usage() {
    cat <<EOF
Использование:
  $PROG --image ФАЙЛ              собрать образ установочного диска в файл
  $PROG --dd /dev/sdX --image Ф    собрать образ и залить его на диск
  $PROG /dev/sdX                   собрать образ и залить его на диск
  $PROG --fs-image ФАЙЛ           свой образ VITAFS (по умолчанию $FS_IMAGE)
  $PROG --kernel ФАЙЛ             свой ELF ядра
  $PROG --size ФАЙЛ               размер образа; по умолчанию - по размеру ФС
  $PROG --yes                     без подтверждения (для скриптов)

Раскладка диска (числа из $LAYOUT):
  LBA 0        stage1 + таблица разделов
  LBA 1..16    stage2
  LBA 256..    ядро
  LBA 32768..  раздел VITAFS с /etc/installer

Требуются: nasm, python3, coreutils. root - только для записи на устройство.
EOF
}

###############################################################################
# 1. Аргументы
###############################################################################
while [ $# -gt 0 ]; do
    case "$1" in
        --image)    OUT_IMAGE="$2"; shift 2 ;;
        --dd)       DEVICE="$2"; DO_DD=1; shift 2 ;;
        --fs-image) FS_IMAGE="$2"; shift 2 ;;
        --kernel)   KERNEL="$2"; shift 2 ;;
        --size)     IMG_SIZE="$2"; shift 2 ;;
        --yes|-y)   ASSUME_YES=1; shift ;;
        -h|--help)  usage; exit 0 ;;
        -*)         usage; die "неизвестный ключ: $1" ;;
        *)          DEVICE="$1"; shift ;;
    esac
done

if [ -z "$OUT_IMAGE" ] && [ -z "$DEVICE" ]; then usage; exit 1; fi
if [ "$DO_DD" -eq 1 ] && [ -z "$OUT_IMAGE" ]; then
    die "--dd требует --image: образ нужно собрать заранее"
fi

###############################################################################
# 2. Инструменты
###############################################################################
for tool in nasm python3 dd truncate stat; do
    command -v "$tool" >/dev/null || die "$tool не найден"
done

###############################################################################
# 3. Раскладка из layout.inc
#
# Единственный источник правды - src/bootldr/layout.inc: его же include'ят
# stage1.asm и stage2.asm. Дублировать числа здесь нельзя, иначе установщик
# разъедется с загрузчиком без единой ошибки компиляции.
###############################################################################
[ -f "$LAYOUT" ] || die "нет $LAYOUT"

layout() {
    # layout ИМЯ -> значение equ из layout.inc
    local name="$1" line
    line="$(grep -E "^[[:space:]]*$name[[:space:]]+equ" "$LAYOUT" | head -1)" \
        || die "в $LAYOUT нет константы $name"
    echo "${line#*equ}" | awk '{print $1}'
}

STAGE2_LBA="$(layout STAGE2_LBA)"
STAGE2_SECTORS="$(layout STAGE2_SECTORS)"
KERNEL_LBA="$(layout KERNEL_LBA)"
PART1_LBA="$(layout PART1_LBA)"

###############################################################################
# 4. Сборка загрузчика и проверки
###############################################################################
if [ ! -f "$KERNEL" ] || [ ! -f "$FS_IMAGE" ]; then
    echo "Нет $KERNEL или $FS_IMAGE, запускаю сборку..."
    ./scripts/build.sh
fi
[ -f "$KERNEL" ]   || die "нет ядра $KERNEL"
[ -f "$FS_IMAGE" ] || die "нет образа VITAFS $FS_IMAGE"
[ -d "$BOOTLDR" ]  || die "нет каталога $BOOTLDR"
[ -f "$MBR_MAKER" ] || die "нет $MBR_MAKER"

kernel_sectors=$(( ($(stat -c%s "$KERNEL") + SECTOR - 1) / SECTOR ))
kernel_end=$((KERNEL_LBA + kernel_sectors))
[ "$kernel_end" -le "$PART1_LBA" ] \
    || die "ядро не помещается до раздела: конец LBA $kernel_end, раздел с LBA $PART1_LBA"

fs_bytes="$(stat -c%s "$FS_IMAGE")"
fs_sectors=$(( (fs_bytes + SECTOR - 1) / SECTOR ))
disk_sectors=$((PART1_LBA + fs_sectors))
disk_bytes=$((disk_sectors * SECTOR))

# Предварительная проверка образа ФС: установщик запускается по маркеру
# /etc/installer, а ядро копируется из /boot. Если чего-то нет, диск
# соберётся, но загрузится обычная система - ловим это здесь.
if [ -f tools/dumpfs.py ]; then
    tree="$(python3 tools/dumpfs.py "$FS_IMAGE" 2>&1)" \
        || die "не читается образ VITAFS: $FS_IMAGE"
    echo "$tree" | grep -q "VITAFS1" \
        || die "$FS_IMAGE - не VITAFS (нет сигнатуры)"
    echo "$tree" | grep -q "/etc/installer" \
        || die "в $FS_IMAGE нет /etc/installer: это обычный образ, а не установочный (берите target/installer.img)"
    echo "$tree" | grep -q "/boot/vitamin_os26.elf" \
        || die "в $FS_IMAGE нет /boot/vitamin_os26.elf: ядро неоткуда будет скопировано на цель"
    info "образ VITAFS: $((fs_bytes / 1024 / 1024)) МиБ, маркер и ядро на месте"
fi

TMP="$(mktemp -d /tmp/vitamin-inst.XXXXXX)"

info "собираю stage1/stage2..."
for st in stage1 stage2; do
    nasm -I "$BOOTLDR" -f bin "$BOOTLDR/$st.asm" -o "$TMP/$st.bin" \
        || die "nasm не собрал $st.asm"
done
stage1_size="$(stat -c%s "$TMP/stage1.bin")"
[ "$stage1_size" -eq 512 ] \
    || die "stage1 должен быть ровно 512 байт (это MBR целиком), получилось $stage1_size"
stage2_size="$(stat -c%s "$TMP/stage2.bin")"
[ "$stage2_size" -le $((STAGE2_SECTORS * SECTOR)) ] \
    || die "stage2 не влезает в $STAGE2_SECTORS секторов ($stage2_size байт)"

info "собираю MBR (stage1 + таблица разделов)..."
python3 "$MBR_MAKER" "$TMP/stage1.bin" "$TMP/mbr.bin" "$PART1_LBA" "$fs_sectors" \
    || die "не собрался MBR"

###############################################################################
# 5. Устройство: показываем, что собираемся стереть, и спрашиваем
###############################################################################
if [ -n "$DEVICE" ]; then
    [ -b "$DEVICE" ] || die "$DEVICE не является блочным устройством"
    # Самая дорогая ошибка здесь - записать поверх живой системы. Сверяем не
    # только корень, но и типичные отдельные точки монтирования: /boot и
    # /home часто на разделах того же диска.
    ROOT_SRC="$(findmnt -no SOURCE / 2>/dev/null || true)"
    for mnt in /boot /home /efi /var; do
        src="$(findmnt -no SOURCE "$mnt" 2>/dev/null || true)"
        [ -n "$src" ] && ROOT_SRC="$ROOT_SRC $src"
    done
    for src in $ROOT_SRC; do
        base="/dev/$(basename "$src" | sed 's/p[0-9]*$//')"
        if [ "$base" = "$DEVICE" ]; then
            die "$DEVICE - системный диск ($src примонтирован). Сюда не пишем."
        fi
    done
    mounted="$(lsblk -nrpo MOUNTPOINT "$DEVICE" 2>/dev/null | grep -v '^$' || true)"
    [ -z "$mounted" ] || die "$DEVICE: на нём смонтировано $(echo "$mounted"). Сначала отмонтируйте."

    echo "================================="
    echo "Устройство: $DEVICE"
    lsblk -dno SIZE,MODEL,TRAN "$DEVICE" 2>/dev/null | sed 's/^/  /' || true
    echo "  ВНИМАНИЕ: содержимое устройства будет стёрто."
    echo "================================="
    if [ "$ASSUME_YES" -eq 0 ]; then
        printf "Введите путь устройства ещё раз для подтверждения: "
        read -r answer
        [ "$answer" = "$DEVICE" ] || die "подтверждение не совпало, ничего не тронуто"
    fi
    dev_bytes="$(blockdev --getsize64 "$DEVICE")"
    [ "$dev_bytes" -ge "$disk_bytes" ] \
        || die "устройство меньше нужного: $((dev_bytes / 1024 / 1024)) МиБ, нужно $((disk_bytes / 1024 / 1024)) МиБ"
fi

###############################################################################
# 6. Сборка образа
###############################################################################
write_at() {
    # write_at LBA ФАЙЛ
    dd if="$2" of="$DISK" bs=512 seek="$1" conv=notrunc status=none \
        || die "не удалось записать $(basename "$2") на LBA $1"
}

echo ""
echo "Раскладка $((disk_bytes / 1024 / 1024)) МиБ:"
echo "  LBA 0        MBR: stage1 + раздел VITAFS"
echo "  LBA $STAGE2_LBA..$((STAGE2_LBA + STAGE2_SECTORS - 1))  stage2 ($stage2_size байт)"
echo "  LBA $KERNEL_LBA..$((kernel_end - 1))  ядро ($(( $(stat -c%s "$KERNEL") / 1024 / 1024 )) МиБ)"
echo "  LBA $PART1_LBA..$((disk_sectors - 1))  VITAFS ($((fs_bytes / 1024 / 1024)) МиБ)"

# Образ файловый и разреженный: на носитель ляжет ровно столько, сколько
# записано, плюс хвост под будущие разделы.
if [ -n "$OUT_IMAGE" ]; then
    mkdir -p "$(dirname "$OUT_IMAGE")"
    rm -f "$OUT_IMAGE"
    DISK="$OUT_IMAGE"
    [ -n "$IMG_SIZE" ] || IMG_SIZE="$((disk_bytes + 4 * 1024 * 1024))"
    truncate -s "$IMG_SIZE" "$DISK"
elif [ "$IMG_SIZE" ]; then
    die "--size имеет смысл только вместе с --image"
fi

write_at 0            "$TMP/mbr.bin"
write_at "$STAGE2_LBA" "$TMP/stage2.bin"
write_at "$KERNEL_LBA" "$KERNEL"
write_at "$PART1_LBA"  "$FS_IMAGE"
sync

###############################################################################
# 7. Готово
###############################################################################
echo ""
echo "================================="
if [ -n "$OUT_IMAGE" ]; then
    echo "Образ: $OUT_IMAGE"
fi
if [ "$DO_DD" -eq 1 ]; then
    img_bytes="$(stat -c%s "$OUT_IMAGE")"
    dev_bytes="$(blockdev --getsize64 "$DEVICE" 2>/dev/null || echo 0)"
    [ "$img_bytes" -le "$dev_bytes" ] \
        || die "образ $((img_bytes / 1024 / 1024)) МиБ не влезает в $DEVICE ($((dev_bytes / 1024 / 1024)) МиБ)"
    echo "Заливаю $OUT_IMAGE на $DEVICE..."
    dd if="$OUT_IMAGE" of="$DEVICE" bs=4M conv=fsync status=progress \
        || die "dd не смог залить образ"
    sync
    echo "Записано на $DEVICE."
    echo "В BIOS целевой машины поставьте этот диск первым загрузочным."
else
    echo "Вставьте носитель в целевую машину и выберите его в BIOS первым."
fi
cat <<'EOF'

Дальше на экране появится меню VitaminOS26 Installer:
  Tab      переключить список дисков и программ
  стрелки  выбрать диск или программу
  Space    снять/поставить программу в /bin
  Enter    перейти к установке
  Esc      отменить

Перед стиранием будет вопрос "Стереть ... и установить VitaminOS26?".
Установка идёт на ДРУГОЙ диск, чем тот, с которого вы загрузились, поэтому
нужны два накопителя: один несёт установщик, второй становится системным.
EOF
echo "================================="