#!/bin/bash
# Запись VitaminOS26 на флешку (или в образ, который потом можно залить dd).
#
#   scripts/make-usb.sh /dev/sda              живая (live) флешка
#   scripts/make-usb.sh --installer /dev/sda  установочная флешка
#   scripts/make-usb.sh --image target/os-usb.img
#   scripts/make-usb.sh --dd /dev/sda --image target/os-usb.img
#
# Нужен root: разметка, mkfs и grub-install работают только с блочным
# устройством. grub-install не умеет ставиться в файл-образ - для режима
# --image мы временно подвешиваем образ как loop-устройство.
#
# Раскладка:
#   p1  VITAFS (сырой образ target/os.img или target/installer.img)
#   p2  FAT с /boot/vitamin_os26.elf и /boot/grub/grub.cfg (GRUB)
# GRUB грузит ядро с p2, ядро монтирует VITAFS с p1. Собственный загрузчик
# ОС на флешке не используется: он ставится только на установленную систему.
set -e

PROG="$(basename "$0")"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

KERNEL="target/x86_64-unknown-none/debug/vitamin_os26"
GRUBCFG="grub.cfg"
FS_IMAGE="target/os.img"
INSTALLER=0
LABEL="VITAMON"
# 1 МиБ до первого раздела: там помещается core.img GRUB.
FS_START=2048
# Модули GRUB. Полный набор (grub-install по умолчанию встраивает все 325
# модулей) не влезает в промежуток до первого раздела, поэтому берём ровно
# то, что нужно для входа в меню и загрузки ядра через multiboot2.
MODULES="biosdisk part_msdos fat normal configfile echo multiboot2 \
search_fs_uuid search_label ls test cat boot sleep date datehook gettext"

DEVICE=""
IMAGE=""
DO_DD=0
ASSUME_YES=0
MOUNT_DIR=""
IMG_SIZE="4G"
LOOP_DEV=""

die() { echo "ОШИБКА: $*" >&2; exit 1; }
info() { echo "  $*"; }

cleanup() {
    [ -n "$MOUNT_DIR" ] && mountpoint -q "$MOUNT_DIR" 2>/dev/null && umount "$MOUNT_DIR"
    [ -n "$LOOP_DEV" ] && losetup -d "$LOOP_DEV" 2>/dev/null
    return 0
}
trap cleanup EXIT

usage() {
    cat <<EOF
Использование:
  $PROG /dev/sdX                 записать на физическое устройство
  $PROG --image ФАЙЛ             собрать образ флешки в файл
  $PROG --dd /dev/sdX --image ФАЙЛ
                                 собрать образ и залить его на флешку
  $PROG --installer ...          установочная флешка (VITAFS + маркер)
  $PROG --fs-image ФАЙЛ          свой образ VITAFS (по умолчанию target/os.img)
  $PROG --kernel ФАЙЛ            свой ELF вместо стандартной сборки
  $PROG --image ФАЙЛ --size 8G   размер образа флешки (по умолчанию 4G)
  $PROG --yes ...                без подтверждения (для скриптов)

Раскладка:
  p1 VITAFS (target/os.img или target/installer.img)
  p2 FAT с /boot/vitamin_os26.elf и /boot/grub/grub.cfg (GRUB)
GRUB грузит ядро с p2, ядро монтирует VITAFS с p1.

Требуются: sfdisk (util-linux), mkfs.vfat (dosfstools), grub-install (grub2).
EOF
}

###############################################################################
# 1. Аргументы
###############################################################################
while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --dd)    DEVICE="$2"; DO_DD=1; shift 2 ;;
        --yes|-y) ASSUME_YES=1; shift ;;
        --kernel) KERNEL="$2"; shift 2 ;;
        --fs-image) FS_IMAGE="$2"; shift 2 ;;
        --installer) INSTALLER=1; shift ;;
        --size)  IMG_SIZE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage; die "неизвестный ключ: $1" ;;
        *)  DEVICE="$1"; shift ;;
    esac
done

if [ -z "$DEVICE" ] && [ -z "$IMAGE" ]; then usage; exit 1; fi

# Установочная флешка отличается только образом VITAFS (в нём маркер
# /etc/installer и ядро в /boot) и заголовком меню GRUB.
if [ "$INSTALLER" -eq 1 ]; then
    [ "$FS_IMAGE" = "target/os.img" ] && FS_IMAGE="target/installer.img"
    [ "$GRUBCFG" = "grub.cfg" ] && GRUBCFG="grub-installer.cfg"
fi

###############################################################################
# 2. Инструменты
###############################################################################
for tool in sfdisk mkfs.vfat grub-install mount umount mountpoint lsblk \
           blockdev partprobe losetup findmnt dd stat truncate; do
    command -v "$tool" >/dev/null || die "$tool не найден (нужен пакет: $(case $tool in
        mkfs.vfat) echo dosfstools ;;
        grub-install) echo grub2 ;;
        sfdisk|lsblk|blockdev|partprobe|mount|umount|mountpoint|findmnt|losetup|dd|stat|truncate) echo util-linux/coreutils ;;
    esac))"
done

if [ "$DO_DD" -eq 1 ]; then
    [ -n "$IMAGE" ] || die "--dd требует --image: сначала собрать образ"
    [ -f "$IMAGE" ] || die "образа $IMAGE нет: соберите его заранее (--image)"
fi

if [ ! -f "$KERNEL" ] || [ ! -f "$FS_IMAGE" ]; then
    echo "Нет $KERNEL или $FS_IMAGE, запускаю сборку..."
    ./scripts/build.sh
fi
[ -f "$FS_IMAGE" ] || die "образ VITAFS $FS_IMAGE не найден"
[ -f "$GRUBCFG" ] || die "конфиг GRUB $GRUBCFG не найден"

###############################################################################
# 3. Диск: показываем, что собираемся стереть, и спрашиваем подтверждения
###############################################################################
if [ -n "$DEVICE" ]; then
    [ -b "$DEVICE" ] || die "$DEVICE не является блочным устройством"
    # Отказ грузить систему с перезаписываемого устройства - самая дорогая
    # ошибка, которую здесь можно допустить. Сверяем не только корень, но и
    # все точки монтирования: /boot и /home часто на разделах того же диска.
    ROOT_SRC="$(findmnt -no SOURCE / 2>/dev/null || true)"
    for mnt in /boot /home /efi /var; do
        src="$(findmnt -no SOURCE "$mnt" 2>/dev/null || true)"
        [ -n "$src" ] && ROOT_SRC="$ROOT_SRC $src"
    done
    for src in $ROOT_SRC; do
        base="/dev/$(basename "$src" | sed 's/p[0-9]*$//')"
        if [ "$base" = "$DEVICE" ]; then
            die "$DEVICE - системный диск ($src примонтирован). Установщик сюда не пишем."
        fi
    done
    mounted="$(lsblk -nrpo MOUNTPOINT "$DEVICE" 2>/dev/null | grep -v '^$' || true)"
    [ -z "$mounted" ] || die "$DEVICE: на нём смонтировано $(echo $mounted). Сначала отмонтируйте."
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
fi

###############################################################################
# 4. Раскладка
#
# Схема: MBR (core.img GRUB в зазоре), p1 - VITAFS, p2 - FAT с ядром и
# grub.cfg. GRUB читает FAT, ядро получает VITAFS из p1.
###############################################################################
part_dev() {
    # part_dev ПУТЬ НОМЕР -> узел раздела (p1 у loop-устройств, 1 у дисков)
    local d="$1" n="$2"
    if [ -b "${d}p${n}" ]; then echo "${d}p${n}"; else echo "${d}${n}"; fi
}

if [ -n "$IMAGE" ]; then
    mkdir -p "$(dirname "$IMAGE")"
    rm -f "$IMAGE"
    # Файл разреженный: на диске лежит ровно столько, сколько записано.
    truncate -s "$IMG_SIZE" "$IMAGE"
    DISK="$IMAGE"
    DISK_IS_IMAGE=1
else
    DISK="$DEVICE"
    DISK_IS_IMAGE=0
fi

if [ "$DISK_IS_IMAGE" -eq 1 ]; then
    size_bytes="$(stat -c%s "$DISK")"
else
    size_bytes="$(blockdev --getsize64 "$DISK")"
fi

fs_bytes="$(stat -c%s "$FS_IMAGE")"
fs_sectors=$(( (fs_bytes + 511) / 512 ))
fat_start=$(( FS_START + fs_sectors ))
# Начало FAT выравниваем на 1 МиБ, как это делают обычные разметчики.
fat_start=$(( (fat_start + 2047) / 2048 * 2048 ))
fat_bytes=$(( size_bytes - fat_start * 512 ))
[ "$fat_bytes" -gt $((64 * 1024 * 1024)) ] \
    || die "носитель слишком мал: под FAT остаётся всего $((fat_bytes / 1024 / 1024)) МиБ"

# FAT16 не тянет больше 2 ГиБ, а Linux на крошечных носителях FAT32 не любит.
if [ "$fat_bytes" -lt $((2 * 1024 * 1024 * 1024)) ]; then
    FAT_BITS=16
    PART_TYPE="06"
else
    FAT_BITS=32
    PART_TYPE="0c"
fi

echo ""
echo "Размечаю $DISK ($(($size_bytes / 1024 / 1024)) МиБ) ..."
echo "  p1 VITAFS: $((fs_sectors / 2048)) МиБ с LBA $FS_START"
echo "  p2 FAT$FAT_BITS: с LBA $fat_start"
{
    echo "label: dos"
    echo "unit: sectors"
    echo ""
    echo "start=$FS_START, size=$fs_sectors, type=83"
    echo "start=$fat_start, type=$PART_TYPE, bootable"
} | sfdisk -f "$DISK" > /dev/null

# Кладём VITAFS в p1 сырыми секторами: пишем прямо в носитель/файл по
# смещению, партишн-узел для этого не нужен.
echo "Пишу VITAFS: $FS_IMAGE ..."
if ! dd if="$FS_IMAGE" of="$DISK" bs=512 seek="$FS_START" conv=notrunc,fsync 2>/dev/null; then
    dd if="$FS_IMAGE" of="$DISK" bs=512 seek="$FS_START" conv=notrunc || die "dd не смог записать VITAFS"
fi

if [ "$DISK_IS_IMAGE" -eq 1 ]; then
    # grub-install требует настоящее блочное устройство, поэтому образ
    # подвешиваем на время установки.
    LOOP_DEV="$(losetup --find --show --partscan "$DISK")"
    TARGET_DISK="$LOOP_DEV"
else
    TARGET_DISK="$DISK"
    partprobe "$DISK" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ -b "$(part_dev "$DISK" 2)" ] && break
        sleep 0.3
    done
fi

PART2="$(part_dev "$TARGET_DISK" 2)"
[ -b "$PART2" ] || die "раздел $PART2 не появился"
mkfs.vfat -F "$FAT_BITS" -n "$LABEL" "$PART2" > /dev/null

MOUNT_DIR="$(mktemp -d /tmp/vitamin-usb.XXXXXX)"
mount "$PART2" "$MOUNT_DIR"
info "раздел $PART2 на месте"

###############################################################################
# 5. Файлы
###############################################################################
mkdir -p "$MOUNT_DIR/boot/grub"
cp "$KERNEL" "$MOUNT_DIR/boot/vitamin_os26.elf"
cp "$GRUBCFG" "$MOUNT_DIR/boot/grub/grub.cfg"
info "ядро: boot/vitamin_os26.elf ($(stat -c%s "$MOUNT_DIR/boot/vitamin_os26.elf") байт)"

###############################################################################
# 6. GRUB в MBR
###############################################################################
# Префикс берётся из точки монтирования, поэтому grub-probe сам узнает, какой
# раздел и какой диск получились. Если подсунуть локальный каталог, префикс
# уедет на диск хоста и флешка загрузится в консоль grub>.
echo ""
echo "Ставлю GRUB..."
grub-install --target=i386-pc --recheck \
    --boot-directory="$MOUNT_DIR/boot" \
    --modules="$MODULES" --install-modules="$MODULES" \
    "$TARGET_DISK"

sync
umount "$MOUNT_DIR"
MOUNT_DIR=""
[ -n "$LOOP_DEV" ] && { losetup -d "$LOOP_DEV"; LOOP_DEV=""; }

###############################################################################
# 7. Готово
###############################################################################
echo ""
echo "================================="
echo "Готово."
if [ "$DO_DD" -eq 1 ]; then
    img_bytes="$(stat -c%s "$IMAGE")"
    dev_bytes="$(blockdev --getsize64 "$DEVICE" 2>/dev/null || echo 0)"
    [ "$img_bytes" -le "$dev_bytes" ] || \
        die "образ $((img_bytes / 1024 / 1024)) МиБ не влезает в $DEVICE ($((dev_bytes / 1024 / 1024)) МиБ)"
    echo "Заливаю $IMAGE на $DEVICE (dd)..."
    dd if="$IMAGE" of="$DEVICE" bs=4M conv=fsync status=progress
    sync
    echo "Записано на $DEVICE. Выньте и вставьте в целевой компьютер."
else
    [ -n "$IMAGE" ] && echo "Образ флешки: $IMAGE"
    echo "Теперь можно вставить носитель в целевой компьютер и включить его."
fi
echo "Загрузка с флешки: в BIOS выберите USB как первый загрузочный диск."
echo "================================="
