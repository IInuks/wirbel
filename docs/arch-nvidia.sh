#!/bin/sh
# Installs the NVIDIA userspace into the Arch stratum, 64 bit, 32 bit and Vulkan, in exactly the
# version of the loaded kernel module. Anything else leaves the driver broken, so the script stops
# before it changes anything if Arch does not have that version.
#
#   ~/wirbel/docs/arch-nvidia.sh --pruefen   only looks, needs no root, changes nothing
#   sudo ~/wirbel/docs/arch-nvidia.sh        installs, after you confirmed the plan
#
# Exit: 0 done or check fine, 1 wrong use, 2 the version does not exist in Arch, 3 no driver or no
# network. Nothing has been changed in those cases.
#
# Why the archive and not the repositories: extra and multilib only hold the newest driver. The
# archive holds every build, including the newest, so one lookup covers both.
set -eu

ARCHIVE=https://archive.archlinux.org/packages
CONF=/etc/pacman.conf
PACKAGES="nvidia-utils lib32-nvidia-utils"
# nvidia-utils pulls in the EGL parts and Mesa stays, libglvnd picks per GPU.
EXTRA="vulkan-icd-loader lib32-vulkan-icd-loader lib32-libglvnd"

check_only=
case "${1:-}" in
    --pruefen) check_only=1 ;;
    "") ;;
    *) echo "Aufruf: $0 [--pruefen]" >&2; exit 1 ;;
esac

in_arch() { strat arch "$@"; }

# Prints the newest build of the wanted version, as a file name in the archive.
archive_file() {
    first=$(printf %s "$1" | cut -c1)
    pattern=$(printf %s "$want" | sed 's/\./\\./g')
    listing=$(curl -fsS -m 30 "$ARCHIVE/$first/$1/") || {
        echo "Das Arch-Archiv ist nicht erreichbar, nichts geändert." >&2
        exit 3
    }
    printf '%s\n' "$listing" |
        grep -oE ">$1-$pattern-[0-9]+-x86_64\.pkg\.tar\.zst<" |
        tr -d '<>' | sort -V | tail -n 1
}

no_version() {
    cat >&2 <<T

ABBRUCH, nichts geändert: Arch hat $1 nicht in Version $want.
Das geladene Modul ist $want, die Bibliotheken müssen genau dazu passen.

Mögliche Wege, die Entscheidung liegt bei dir:
  1. Void-Treiber und Kernel-Modul auf die Version von Arch heben ($(in_arch pacman -Si nvidia-utils 2>/dev/null | sed -n 's/^Version *: //p')),
     dann dieses Skript nochmal starten. Geht nur, wenn Void diese Version anbietet.
  2. Warten, bis beide dieselbe Version haben: Dieses Skript passt sich dem geladenen Modul an.
  3. Eigenes Arch-Paket aus NVIDIAs Originaldatei bauen:
     https://download.nvidia.com/XFree86/Linux-x86_64/$want/NVIDIA-Linux-x86_64-$want.run
     Das wäre nicht mehr von Arch gepflegt.
Solange bleibt alles wie bisher: Clients aus Arch rendern auf der AMD-iGPU, Void hat den Treiber.
T
    exit 2
}

[ -r /proc/driver/nvidia/version ] || {
    echo "Kein NVIDIA-Modul geladen, nichts geändert." >&2
    exit 3
}
want=$(sed -n 's/^NVRM version:.*  \([0-9][0-9.]*\)  .*/\1/p' /proc/driver/nvidia/version)
[ -n "$want" ] || {
    echo "Die Modul-Version war nicht zu lesen, nichts geändert." >&2
    exit 3
}
echo "Geladenes Modul: $want"

urls=
for package in $PACKAGES; do
    file=$(archive_file "$package")
    [ -n "$file" ] || no_version "$package"
    echo "Gefunden: $file"
    urls="$urls $ARCHIVE/$(printf %s "$package" | cut -c1)/$package/$file"
done

multilib_off=
in_arch grep -q '^\[multilib\]' "$CONF" || multilib_off=1
pin_missing=
in_arch grep -qE '^IgnorePkg.*nvidia-utils' "$CONF" || pin_missing=1

echo
echo "Plan für das Arch-Stratum:"
[ -z "$multilib_off" ] || echo "  - [multilib] in $CONF einschalten, Sicherung nach $CONF.vor-nvidia"
[ -z "$pin_missing" ] || echo "  - IgnorePkg = $PACKAGES: Sonst zieht das nächste pacman -Syu den Treiber am Modul vorbei"
echo "  - pacman -Sy, nur die Paketlisten, kein Update des Systems"
echo "  - pacman -U$urls" | sed 's/ https/\n      https/g'
echo "  - pacman -S --needed --asdeps $EXTRA"
cat <<T

Danach gilt: Programme aus Arch sehen die NVIDIA als Vulkan-Gerät, und libglvnd fragt NVIDIA zuerst
(10_nvidia.json vor 50_mesa.json). Zurück geht es mit: strat arch pacman -Rns $PACKAGES
T

[ -z "$check_only" ] || exit 0
[ "$(id -u)" = 0 ] || {
    echo "Zum Installieren mit sudo starten." >&2
    exit 1
}
printf 'Installieren? Nur "ja" macht weiter: '
read -r answer
[ "$answer" = ja ] || {
    echo "Nichts geändert."
    exit 0
}

if [ -n "$multilib_off$pin_missing" ]; then
    in_arch cp -n "$CONF" "$CONF.vor-nvidia"
fi
if [ -n "$multilib_off" ]; then
    in_arch sed -i '/^#\[multilib\]$/,/^#Include/s/^#//' "$CONF"
fi
if [ -n "$pin_missing" ]; then
    in_arch sed -i "/^\[options\]/a IgnorePkg = $PACKAGES" "$CONF"
fi
in_arch pacman -Sy
# shellcheck disable=SC2086 # one word per package
in_arch pacman -U --noconfirm $urls
# shellcheck disable=SC2086
in_arch pacman -S --needed --asdeps --noconfirm $EXTRA

got=$(in_arch pacman -Q nvidia-utils | cut -d' ' -f2 | cut -d- -f1)
[ "$got" = "$want" ] || {
    echo "Installiert ist $got, das Modul ist $want. Das sollte nicht sein, bitte melden." >&2
    exit 1
}
echo "Fertig: nvidia-utils $got passt zum Modul."
