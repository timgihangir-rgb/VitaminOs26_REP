#!/bin/bash
# Запись VitaminOS26 на флешку (или в образ, который потом можно залить dd).
#
#   scripts/make-usb.sh /dev/sda
#   scripts/make-usb.sh --image target/os-usb.img
#   scripts/make-usb.sh --dd /dev/sda --image target/os-usb.img
#
# Нужен root: разметка, форматирование и grub-install работают только с
# блочным устройством. grub-install не умеет ставиться в файл-образ - для
# режима --image мы временно подвешиваем образ как loop-устройство.
set -e

PROG="$(basename "$0")"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

KERNEL="target/x86_64-unknown-none/debug/vitamin_os26"
GRUBCFG="grub.cfg"
LABEL="VITAMON"
START_SECTOR=2048                  # 1 МиБ - чтобы GPT/суперблок не мешали
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

die() { echo "ОШИБКА: $*" >&2; exit 1; }
info() { echo "  $*"; }

cleanup() {
    [ -n "$MOUNT_DIR" ] && mountpoint -q "$MOUNT_DIR" 2>/dev/null && umount "$MOUNT_DIR"
    [ -n "$LOOP_DEV" ] && losetup -d "$LOOP_DEV" 2>/dev/null
    return 0
}
LOOP_DEV=""
trap cleanup EXIT

usage() {
    cat <<EOF
Использование:
  $PROG /dev/sdX                 записать на физическое устройство
  $PROG --image ФАЙЛ             собрать образ флешки в файл
  $PROG --dd /dev/sdX --image ФАЙЛ
                                 собрать образ и залить его на флешку
  $PROG --yes ...                без подтверждения (для скриптов)
  $PROG --kernel ФАЙЛ            свой ELF вместо стандартной сборки
  $PROG --image ФАЙЛ --size 8G   размер образа флешки (по умолчанию 4G)

Требуются: sfdisk (util-linux), mkfs.ffat (dosfstools), grub-install (grub2).
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
        --size)  IMG_SIZE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage; die "неизвестный ключ: $1" ;;
        *)  DEVICE="$1"; shift ;;
    esac
done

if [ -z "$DEVICE" ] && [ -z "$IMAGE" ]; then usage; exit 1; fi

###############################################################################
# 2. Инструменты
###############################################################################
for tool in sfdisk mkfs.vfat grub-install mount umount mountpoint lsblk \
           blockdev partprobe losetup findmnt; do
    command -v "$tool" >/dev/null || die "$tool не найден (нужен пакет: $(case $tool in
        mkfs.vfat) echo dosfstools ;;
        grub-install) echo grub2 ;;
        sfdisk|lsblk|blockdev|partprobe|mount|umount|mountpoint|findmnt|losetup) echo util-linux ;;
    esac))"
done

if [ "$DO_DD" -eq 1 ]; then
    [ -n "$IMAGE" ] || die "--dd требует --image: сначала собрать образ"
    [ -f "$IMAGE" ] || die "образа $IMAGE нет: соберите его заранее (--image)"
    img_bytes="$(stat -c%s "$IMAGE")"
    dev_bytes="$(blockdev --getsize64 "$DEVICE" 2>/dev/null || echo 0)"
    [ "$img_bytes" -le "$dev_bytes" ] || \
        die "образ $((img_bytes / 1024 / 1024)) МиБ не влезает в $DEVICE ($((dev_bytes / 1024 / 1024)) МиБ)"
fi

if [ ! -f "$KERNEL" ]; then
    echo "Ядро $KERNEL не найдено, запускаю сборку..."
    ./scripts/build.sh
fi

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
# Схема одна: MBR (446 байт кода GRUB + таблица разделов + 0x55AA), с 1-го
# сектора - core.img, дальше раздел FAT32, в нём /boot с ядром и grub.cfg.
# Собственный загрузчик ОС для установленной системы живёт в другом месте
# диска (см. src/bootldr) и к флешке отношения не имеет.
###############################################################################
part_dev() {
    # part_dev ПУТЬ -> раздел в пути (для loop-устройств с суффиксом p1)
    local d="$1"
    if [ -b "${d}p1" ]; then echo "${d}p1"; else echo "${d}1"; fi
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

# Тип раздела и разрядность FAT зависят от размера: FAT16 не тянет больше
# 2 ГиБ, а Linux на крошечных носителях FAT32 не любит.
# blockdev умеет только про настоящие устройства, у файла-образа размер
# берём через stat.
if [ "$DISK_IS_IMAGE" -eq 1 ]; then
    size_bytes="$(stat -c%s "$DISK")"
else
    size_bytes="$(blockdev --getsize64 "$DISK")"
fi
if [ "$size_bytes" -lt $((2 * 1024 * 1024 * 1024)) ]; then
    FAT_BITS=16
    PART_TYPE="06"
else
    FAT_BITS=32
    PART_TYPE="0c"
fi

echo ""
echo "Размечаю $DISK ($(($size_bytes / 1024 / 1024)) МиБ, FAT$FAT_BITS) ..."
printf "label: dos\nunit: sectors\n\nstart=$START_SECTOR, type=$PART_TYPE, bootable\n" \
    | sfdisk -f "$DISK" >/dev/null
partprobe "$DISK" 2>/dev/null || true

if [ "$DISK_IS_IMAGE" -eq 1 ]; then
    # grub-install требует настоящее блочное устройство, поэтому образ
    # подвешиваем на время установки.
    LOOP_DEV="$(losetup --find --show --partscan "$DISK")"
    TARGET_DISK="$LOOP_DEV"
    # mkfs.fat --offset измеряется в секторах по 512 байт.
    mkfs.vfat -F "$FAT_BITS" -n "$LABEL" --offset "$START_SECTOR" "$DISK"
else
    TARGET_DISK="$DISK"
    # Узел раздела появляется после перечитывания таблицы разделов; на
    # некоторых системах это занимает мгновение после выхода sfdisk.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ -b "$(part_dev "$DISK")" ] && break
        sleep 0.3
    done
    mkfs.vfat -F "$FAT_BITS" -n "$LABEL" "$(part_dev "$DISK")"
fi

PART="$(part_dev "$TARGET_DISK")"
MOUNT_DIR="$(mktemp -d /tmp/vitamin-usb.XXXXXX)"
mount "$PART" "$MOUNT_DIR"
info "раздел $PART на месте"

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