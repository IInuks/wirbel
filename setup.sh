#!/bin/bash
# Sets up wirbel on a new machine: packages, Rust, the build, the session entry and a configuration.
# Supported: Arch, Debian 13 or newer, Ubuntu, Void, and what derives from them. Run it as your user,
# it asks for sudo where it has to.
#
#   ./setup.sh --check           changes nothing, shows what is missing and what would happen
#   ./setup.sh                   sets up, after you confirmed the list of packages
#   ./setup.sh --yes             the same without the question
#   ./setup.sh --layout LAYOUT   keyboard layout, like de or us. Default: the one of the system
#   ./setup.sh --edit-config     opens the configuration in your editor before it is installed
#   ./setup.sh --config FILE     installs your own file instead of the generated one
#   ./setup.sh --replace-config  replaces an existing configuration, with a backup
#   ./setup.sh --remove          takes wirbel out again, your configuration stays
#
# The two ways to bring your own configuration replace an existing one, also with a backup, and work
# together. All options are in src/default.conf. Without --yes the first run asks if you want to edit.
# Run it again after a `git pull` to rebuild. Bedrock has its own way: docs/install-session.sh.
set -u

ROOT=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
LIB=$HOME/.local/lib/wirbel
BIN_DIR=$HOME/.local/bin
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}
SESSIONS=/usr/share/wayland-sessions
STAMP=$(date +%Y%m%d-%H%M%S)
NEED_RUST=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml")
BUILT=$ROOT/target/release/wirbel

CHECK=
YES=
LAYOUT=
REPLACE=
EDIT=
CONFIG_FROM=
REMOVE=
ALLOW_ROOT=
CARGO=cargo
SUDO=sudo

say() { echo "==> $*"; }
info() { echo "    $*"; }
die() { echo "setup.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case $1 in
    --check) CHECK=1 ;;
    --yes | -y) YES=1 ;;
    --layout)
        [ $# -ge 2 ] || die "--layout needs a value"
        LAYOUT=$2
        shift
        ;;
    --replace-config) REPLACE=1 ;;
    --edit-config) EDIT=1 ;;
    --config)
        [ $# -ge 2 ] || die "--config needs a file"
        CONFIG_FROM=$2
        shift
        ;;
    --remove) REMOVE=1 ;;
    # For containers. A session of root is of no use on a real machine.
    --allow-root) ALLOW_ROOT=1 ;;
    -h | --help)
        sed -n '2,/^# Run it again/p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *) die "unknown argument $1, see --help" ;;
    esac
    shift
done

if [ "$(id -u)" = 0 ]; then
    [ -n "$ALLOW_ROOT" ] || die "run this as your own user, it uses sudo where it has to"
    SUDO=
fi
if [ -e /bedrock/etc/bedrock-release ] || [ -x /bedrock/bin/strat ]; then
    die "this is Bedrock Linux: build with cargo, then use docs/install-session.sh"
fi
[ -r /etc/os-release ] || die "no /etc/os-release, cannot tell the distribution"
# shellcheck disable=SC1091
. /etc/os-release

# A family from ID and ID_LIKE. Package names are checked against the repositories of each.
FAMILY=
for id in ${ID:-} ${ID_LIKE:-}; do
    case $id in
    arch) FAMILY=arch ;;
    debian | ubuntu) FAMILY=debian ;;
    void) FAMILY=void ;;
    esac
    [ -z "$FAMILY" ] || break
done

packages() {
    # Build first, then the session, then the desktop of session/config.extra.
    case $FAMILY in
    arch)
        echo base-devel git pkgconf ca-certificates \
            libinput mesa libdrm seatd libxkbcommon wayland pixman \
            xorg-xwayland dbus pipewire wireplumber pipewire-pulse \
            xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk \
            kitty fuzzel waybar mako grim slurp wl-clipboard polkit-gnome ttf-dejavu adwaita-cursors
        have_rust || echo rust
        ;;
    debian)
        echo build-essential git pkg-config ca-certificates \
            libinput-dev libgbm-dev libdrm-dev libseat-dev libxkbcommon-dev libwayland-dev \
            libpixman-1-dev libudev-dev libegl-dev libgles-dev libgl1-mesa-dri libegl-mesa0 \
            xwayland dbus-user-session dbus-bin pipewire wireplumber pipewire-pulse \
            xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk \
            kitty fuzzel waybar mako-notifier grim slurp wl-clipboard mate-polkit \
            fonts-dejavu-core adwaita-icon-theme
        # Debian 13 has Rust 1.85, wirbel needs more, so it comes from rustup.
        have_rust || echo rustup
        ;;
    void)
        echo base-devel git pkg-config ca-certificates \
            libinput-devel MesaLib-devel libdrm-devel libseat-devel libxkbcommon-devel \
            wayland-devel pixman-devel eudev-libudev-devel mesa-dri \
            xorg-server-xwayland dbus elogind pipewire wireplumber \
            xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk \
            kitty fuzzel Waybar mako grim slurp wl-clipboard polkit polkit-gnome \
            dejavu-fonts-ttf adwaita-icon-theme
        have_rust || echo rust cargo
        ;;
    esac
}

installed() {
    case $FAMILY in
    arch) pacman -Q "$1" >/dev/null 2>&1 || pacman -Qg "$1" >/dev/null 2>&1 ;;
    debian) dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'ok installed' ;;
    void) xbps-query "$1" >/dev/null 2>&1 ;;
    esac
}

install_packages() {
    case $FAMILY in
    # With -y alone a partial upgrade could break the system, and a new one has no package lists yet.
    arch) $SUDO pacman -Syu --needed --noconfirm "$@" ;;
    debian) $SUDO apt-get update && $SUDO apt-get install -y "$@" ;;
    # xbps refuses everything else until it is up to date itself, which is the case on a new system.
    void) $SUDO xbps-install -Syu xbps && $SUDO xbps-install -y "$@" ;;
    esac
}

rust_new_enough() { # COMMAND...
    local version
    version=$("$@" --version 2>/dev/null | awk '{print $2}')
    [ -n "$version" ] && [ "$(printf '%s\n%s\n' "$NEED_RUST" "$version" | sort -V | head -n 1)" = "$NEED_RUST" ]
}

have_rust() { command -v cargo >/dev/null && rust_new_enough rustc; }

setup_rust() {
    export PATH="$HOME/.cargo/bin:$PATH"
    if have_rust; then
        info "Rust: $(rustc --version)"
        return
    fi
    command -v rustup >/dev/null || die "Rust $NEED_RUST or newer is missing and there is no rustup"
    say "Installing Rust with rustup"
    rustup toolchain install stable --profile minimal || die "rustup failed"
    CARGO="rustup run stable cargo"
}

# The layout the system was set up with.
system_layout() {
    local value=
    if command -v localectl >/dev/null; then
        value=$(localectl status 2>/dev/null | sed -n 's/.*X11 Layout: *//p')
    fi
    if [ -z "$value" ] && [ -r /etc/default/keyboard ]; then
        value=$(sed -n 's/^XKBLAYOUT="\{0,1\}\([^"]*\)"\{0,1\}/\1/p' /etc/default/keyboard)
    fi
    if [ -z "$value" ]; then
        # A console keymap, like de-latin1 or KEYMAP="us".
        value=$(sed -n 's/^KEYMAP="\{0,1\}\([a-z]*\).*/\1/p' /etc/vconsole.conf /etc/rc.conf 2>/dev/null | head -n 1)
    fi
    echo "${value:-us}"
}

write_config() { # FILE
    local agent
    # Each distribution puts it elsewhere.
    agent=$(find /usr/lib /usr/libexec -name 'polkit-*-authentication-agent-1' -type f 2>/dev/null | head -n 1)
    {
        sed "s/^keyboard_layout = .*/keyboard_layout = $LAYOUT/" "$ROOT/src/default.conf"
        cat "$ROOT/session/config.extra"
        [ -z "$agent" ] || echo "exec-once = $agent"
    } >"$1"
}

config_errors() { # FILE
    "$BUILT" check "$1" 2>&1 | grep -aE ' line [0-9]+: '
}

editor() {
    local name
    for name in "${VISUAL:-}" "${EDITOR:-}" nano vim vi; do
        [ -n "$name" ] && command -v "${name%% *}" >/dev/null && {
            echo "$name"
            return
        }
    done
}

# wirbel skips a line it does not understand, so keeping such a file is possible, just unlikely.
edit_config() { # FILE
    local report answer
    while :; do
        # shellcheck disable=SC2046 # EDITOR may hold arguments
        $(editor) "$1" || die "the editor failed, nothing of the configuration was installed"
        report=$(config_errors "$1")
        [ -n "$report" ] || return 0
        echo "$report"
        printf 'wirbel will skip these lines. Edit again? [Y/n] '
        read -r answer
        case $answer in
        n | N | no | nein) return 0 ;;
        esac
    done
}

install_config() {
    local target=$CONFIG/wirbel/config fresh report
    fresh=$(mktemp)
    if [ -n "$CONFIG_FROM" ]; then
        cp "$CONFIG_FROM" "$fresh" || die "cannot read $CONFIG_FROM"
    else
        write_config "$fresh"
    fi
    if [ -n "$EDIT" ]; then
        edit_config "$fresh"
    else
        report=$(config_errors "$fresh")
        if [ -n "$report" ]; then
            echo "$report"
            rm -f "$fresh"
            die "the configuration has lines wirbel does not understand"
        fi
    fi
    mkdir -p "$CONFIG/wirbel"
    if [ -e "$target" ] && [ -z "$REPLACE" ]; then
        mv "$fresh" "$target.template"
        info "$target stays as it is, the new one is next to it: $target.template"
        return
    fi
    if [ -e "$target" ]; then
        cp -p "$target" "$target.bak-$STAMP"
        info "Backup: $target.bak-$STAMP"
    fi
    mv "$fresh" "$target"
    chmod 644 "$target"
    if [ -n "$CONFIG_FROM$EDIT" ]; then
        info "$target, your own"
    else
        info "$target, keyboard layout $LAYOUT"
    fi
}

install_session() {
    local entry
    mkdir -p "$LIB" "$BIN_DIR" "$CONFIG/xdg-desktop-portal" || die "cannot write to $LIB"
    # Copy first and rename: A running session keeps its file.
    cp "$BUILT" "$LIB/wirbel.new" && mv -f "$LIB/wirbel.new" "$LIB/wirbel" || die "cannot install wirbel"
    ln -sf wirbel "$LIB/mmsg"
    cp "$ROOT/session/wirbel-session" "$ROOT/session/xdpw.conf" "$LIB/" || die "cannot install the session"
    cp "$ROOT/docs/tty-dbus.conf" "$LIB/bus.conf"
    ln -sf "$LIB/wirbel" "$BIN_DIR/wirbel"
    ln -sf "$LIB/wirbel-session" "$BIN_DIR/wirbel-session"
    cp "$ROOT/session/wirbel-portals.conf" "$CONFIG/xdg-desktop-portal/wirbel-portals.conf"

    entry=$(mktemp)
    sed "s|@HOME@|$HOME|" "$ROOT/session/wirbel.desktop" >"$entry"
    $SUDO mkdir -p "$SESSIONS" && $SUDO install -m 644 "$entry" "$SESSIONS/wirbel.desktop" ||
        die "cannot write $SESSIONS/wirbel.desktop"
    rm -f "$entry"
    info "$SESSIONS/wirbel.desktop"
}

# Reading the keyboard directly is what lets Ctrl+Alt+Shift+Backspace work while wirbel hangs. The
# price: Every program of this user can then read the keyboard too.
missing_groups() {
    local group
    [ "$(id -u)" != 0 ] || return 0
    for group in input video; do
        id -nG | tr ' ' '\n' | grep -qx "$group" || echo "$group"
    done
}

add_groups() {
    local group
    for group in $(missing_groups); do
        $SUDO usermod -aG "$group" "$USER" && info "$USER is now in the group $group, this counts from the next login"
    done
}

# The login managers need these, other systems start them by themselves.
enable_services() {
    [ "$FAMILY" = void ] || return 0
    local service
    for service in dbus elogind; do
        if [ -d "/etc/sv/$service" ] && [ -d /var/service ] && [ ! -e "/var/service/$service" ]; then
            $SUDO ln -s "/etc/sv/$service" /var/service/ && info "Service enabled: $service"
        fi
    done
}

remove() {
    say "Removing wirbel"
    $SUDO rm -f "$SESSIONS/wirbel.desktop"
    rm -f "$BIN_DIR/wirbel" "$BIN_DIR/wirbel-session" "$CONFIG/xdg-desktop-portal/wirbel-portals.conf"
    rm -rf "$LIB"
    info "$CONFIG/wirbel/config stays, packages stay too."
}

if [ -n "$REMOVE" ]; then
    remove
    exit 0
fi

[ -z "$CONFIG_FROM" ] || [ -r "$CONFIG_FROM" ] || die "cannot read $CONFIG_FROM"
# Before anything changed, not after a long build.
if [ -n "$EDIT" ] && [ -z "$CHECK" ]; then
    [ -t 0 ] && [ -t 1 ] || die "--edit-config needs a terminal"
    [ -n "$(editor)" ] || die "no editor found, set EDITOR or use --config FILE"
fi
[ -z "$CONFIG_FROM$EDIT" ] || REPLACE=1

[ -n "$FAMILY" ] || die "${PRETTY_NAME:-this system} is not supported yet. Needed: Rust $NEED_RUST, libinput, libseat, gbm, libdrm, xkbcommon, Xwayland, PipeWire, xdg-desktop-portal-wlr"

missing=()
for package in $(packages); do
    installed "$package" || missing+=("$package")
done
[ -n "$LAYOUT" ] || LAYOUT=$(system_layout)
case $LAYOUT in
*[!a-zA-Z0-9,_-]*) die "that does not look like a keyboard layout: $LAYOUT" ;;
esac

say "${PRETTY_NAME:-$FAMILY}, keyboard layout $LAYOUT"
if [ ${#missing[@]} -gt 0 ]; then
    say "Packages to install (${#missing[@]}): ${missing[*]}"
else
    say "All packages are there"
fi
if [ ! -d /run/systemd/system ] && [ "$FAMILY" != void ]; then
    say "No systemd here. wirbel needs logind or seatd to take over the screen, set that up yourself."
fi
groups=$(missing_groups | xargs)
if [ -n "$groups" ]; then
    say "${USER:-You} will join these groups: $groups. Programs of this user can then read the keyboard, the emergency exit needs that."
fi
if [ -n "$CHECK" ]; then
    say "Would also: build wirbel, install to $LIB, write $SESSIONS/wirbel.desktop and a configuration"
    say "Nothing changed."
    exit 0
fi

if [ -z "$YES" ] && [ ${#missing[@]} -gt 0 ]; then
    printf 'Install these packages? [y/N] '
    read -r answer
    case $answer in
    y | Y | yes | j | J | ja) ;;
    *)
        echo "Nothing changed."
        exit 0
        ;;
    esac
fi
if [ -z "$YES$EDIT$CONFIG_FROM" ] && [ ! -e "$CONFIG/wirbel/config" ] && [ -t 0 ] && [ -n "$(editor)" ]; then
    printf 'Edit the configuration yourself before it is installed? [y/N] '
    read -r answer
    case $answer in
    y | Y | yes | j | J | ja) EDIT=1 ;;
    esac
fi
if [ ${#missing[@]} -gt 0 ]; then
    install_packages "${missing[@]}" || die "installing packages failed"
fi

setup_rust
say "Building, the first time takes a few minutes"
(cd "$ROOT" && $CARGO build --release --locked) || die "the build failed"

say "Installing"
install_session
install_config
add_groups
enable_services

echo
say "Done."
info "Choose wirbel in your login manager, or run wirbel-session on a text console."
case :$PATH: in
*:"$BIN_DIR":*) ;;
*) info "$BIN_DIR is not in your PATH, on a console run $BIN_DIR/wirbel-session" ;;
esac
info "Super+Return terminal, Super+Space launcher, Super+M leave. All keys: $CONFIG/wirbel/config"
info "If it hangs, Ctrl+Alt+Shift+Backspace always ends it. Logs: ~/.local/state/wirbel"
