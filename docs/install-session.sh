#!/bin/bash
# Sets up wirbel as a session in the quickdots greeter. Nur der Nutzer startet das, aus Mango oder
# einem TTY, nach `cargo build --release`:
#
#   docs/install-session.sh --pruefen    aendert nichts, sagt was geht und was passieren wuerde
#   docs/install-session.sh              richtet ein, fragt fuer den Greeter-Eintrag nach sudo
#   docs/install-session.sh --config-ersetzen    wie oben, ersetzt eine vorhandene Config, mit Backup
#   docs/install-session.sh --entfernen  nimmt den Eintrag aus dem Greeter, sonst bleibt alles
#   docs/install-session.sh --lib-nach DIR   nur die Programmdateien nach DIR, fuer docs/tty-run.sh
#
# Mango wird nicht angefasst: Keine Datei von Mango, greetd oder quickdots aendert sich, die Sitzung
# ist nur ein weiterer Eintrag in der Liste des Greeters. Rueckfall: Im Greeter Mango waehlen.
# docs/SESSION.md beschreibt das alles.
ROOT=$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)
BIN=$ROOT/target/release/wirbel
LIB=$HOME/.local/lib/wirbel
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}
# Der Greeter laeuft in Arch, siehe /etc/greetd/config.toml, und liest deshalb Archs Verzeichnis.
SESSIONS=${WIRBEL_SESSIONS_DIR:-/bedrock/strata/arch/usr/share/wayland-sessions}
GREETER_SCRIPT=$HOME/quickdots/greeter/scripts/sessions.sh
STAMP=$(date +%Y%m%d-%H%M%S)
STRAT=/bedrock/bin/strat
FAILED=0

ok() { echo "  ok    $*"; }
bad() { echo "  FEHLT $*"; FAILED=1; }
note() { echo "  info  $*"; }

desktop_entry() { sed "s|@HOME@|$HOME|" "$ROOT/session/wirbel.desktop"; }

make_lib() { # DIR
  mkdir -p "$1" || return 1
  # Erst kopieren, dann umbenennen: Eine laufende Sitzung behaelt ihre Datei.
  cp "$BIN" "$1/wirbel.new" && mv -f "$1/wirbel.new" "$1/wirbel" || return 1
  ln -sf wirbel "$1/mmsg"
  cp "$ROOT/session/wirbel-session" "$ROOT/session/steam" "$ROOT/session/xdpw.conf" "$1/" || return 1
  cp "$ROOT/docs/tty-dbus.conf" "$1/bus.conf"
  # Der Eintrag von Steam nennt /usr/bin/steam, er muss wie `steam` im PATH auf die Sitzung zeigen.
  local entry=/bedrock/strata/void/usr/share/applications/steam.desktop
  if [ -f "$entry" ]; then
    mkdir -p "$1/share/applications"
    sed "s|^Exec=/usr/bin/steam|Exec=$1/steam|" "$entry" > "$1/share/applications/steam.desktop"
  fi
}

fresh_build() {
  [ -x "$BIN" ] && [ -z "$(cd "$ROOT" && find src Cargo.toml Cargo.lock -newer "$BIN" -print -quit)" ]
}

check_tools() { # STRATUM TOOL...
  local stratum=$1 tool; shift
  for tool in "$@"; do
    if "$STRAT" "$stratum" sh -c "command -v '$tool' >/dev/null || [ -x '$tool' ]"; then
      ok "$stratum: $tool"
    else
      bad "$stratum: $tool"
    fi
  done
}

compare() { # WAS ZIEL QUELLE
  if [ ! -e "$2" ]; then note "$1: $2 wird angelegt"
  elif cmp -s "$2" "$3"; then note "$1: $2 ist schon aktuell"
  else note "$1: $2 gibt es schon und ist anders"; fi
}

check() {
  echo "Bauen:"
  if fresh_build; then ok "$BIN ist aktuell"; else bad "$BIN fehlt oder ist aelter als der Quelltext, erst cargo build --release"; fi
  [ -x "$STRAT" ] && ok "Bedrock" || bad "$STRAT"
  if [ -x "$BIN" ] && "$STRAT" void "$BIN" --help >/dev/null 2>&1; then ok "Das Binary laeuft in Void"; else bad "Das Binary startet nicht in Void"; fi

  echo "Programme der Sitzung:"
  check_tools void dbus-run-session dbus-send dbus-update-activation-environment mountpoint pgrep timeout \
    /usr/libexec/polkit-gnome-authentication-agent-1
  check_tools debian pipewire wireplumber pipewire-pulse pw-cli
  check_tools arch /usr/lib/xdg-desktop-portal /usr/lib/xdg-desktop-portal-wlr /usr/lib/xdg-desktop-portal-gtk
  local tool
  for tool in qs awww kitty firefox flameshot grim loginctl; do
    if command -v "$tool" >/dev/null; then ok "$tool"; else bad "$tool"; fi
  done

  echo "Config-Vorlage:"
  local path list
  list=$(grep -oE '\$HOME/[^ ]+' "$ROOT/session/config" | sort -u)
  for path in $list; do
    path=${path/\$HOME/$HOME}
    if [ -e "$path" ]; then ok "$path"; else bad "$path"; fi
  done
  if [ -x "$BIN" ]; then
    # check meldet jede unbrauchbare Zeile als "line N: ...".
    "$STRAT" void "$BIN" check "$ROOT/session/config" 2>&1 | grep -aE ' line [0-9]+: ' > "${TMPDIR:-/tmp}/wirbel-check.$$"
    if [ -s "${TMPDIR:-/tmp}/wirbel-check.$$" ]; then cat "${TMPDIR:-/tmp}/wirbel-check.$$"; bad "Zeilen, die wirbel nicht versteht"; else ok "wirbel versteht jede Zeile"; fi
    rm -f "${TMPDIR:-/tmp}/wirbel-check.$$"
  fi

  echo "Greeter (nur gelesen):"
  [ -d "$SESSIONS" ] && ok "$SESSIONS" || bad "$SESSIONS"
  [ -e "$SESSIONS/mango-dbus.desktop" ] && ok "Rueckfall: Mango steht im Greeter" || bad "$SESSIONS/mango-dbus.desktop fehlt, ohne Rueckfall nicht einrichten"
  if [ -f "$GREETER_SCRIPT" ]; then
    local trial
    trial=$(mktemp -d)
    desktop_entry > "$trial/wirbel.desktop"
    local row
    row=$(sh "$GREETER_SCRIPT" "$trial")
    rm -rf "$trial"
    case $row in
      wirbel$'\t'"$LIB/wirbel-session"$'\t'wirbel\;wlroots$'\t'*) ok "Der Greeter wuerde lesen: $(echo "$row" | cut -f 1,2)" ;;
      *) bad "Der Greeter liest den Eintrag anders: $row" ;;
    esac
  else
    bad "$GREETER_SCRIPT"
  fi
  case $HOME in *' '*) bad "Der Greeter trennt Exec an Leerzeichen, HOME hat eins" ;; esac

  echo "Was die Einrichtung taete:"
  note "Programmdateien: $LIB"
  compare Config "$CONFIG/wirbel/config" "$ROOT/session/config"
  compare Portale "$CONFIG/xdg-desktop-portal/wirbel-portals.conf" "$ROOT/session/wirbel-portals.conf"
  local entry
  entry=$(mktemp)
  desktop_entry > "$entry"
  compare Greeter-Eintrag "$SESSIONS/wirbel.desktop" "$entry"
  rm -f "$entry"
  if mountpoint -q "${XDG_RUNTIME_DIR:-/nonexistent}/doc" || pgrep -u "$(id -u)" -x mango >/dev/null; then
    note "Mango laeuft. Das stoert nicht: Die Einrichtung aendert an der laufenden Sitzung nichts."
  fi
}

backup() { # DATEI
  [ -e "$1" ] && cp -p "$1" "$1.bak-$STAMP" && echo "Backup: $1.bak-$STAMP"
}

# Legt ZIEL an. Eine andere vorhandene Datei bleibt, ausser es ist erlaubt, dann mit Backup.
place() { # QUELLE ZIEL ERSETZEN
  mkdir -p "$(dirname "$2")"
  if [ -e "$2" ] && ! cmp -s "$1" "$2"; then
    if [ "$3" != ja ]; then
      cp "$1" "$2.vorlage"
      echo "$2 bleibt wie es ist, die Vorlage liegt daneben: $2.vorlage"
      return
    fi
    backup "$2"
  fi
  cp "$1" "$2" && echo "Angelegt: $2"
}

install_entry() {
  local entry sudo=sudo
  entry=$(mktemp)
  desktop_entry > "$entry"
  [ -w "$SESSIONS" ] && sudo=
  if [ -e "$SESSIONS/wirbel.desktop" ]; then
    $sudo cp -p "$SESSIONS/wirbel.desktop" "$SESSIONS/wirbel.desktop.bak-$STAMP" || return 1
  fi
  $sudo install -m 644 "$entry" "$SESSIONS/wirbel.desktop" || return 1
  rm -f "$entry"
  echo "Angelegt: $SESSIONS/wirbel.desktop"
}

remove_entry() {
  local sudo=sudo
  [ -w "$SESSIONS" ] && sudo=
  if [ -e "$SESSIONS/wirbel.desktop" ]; then
    $sudo mv "$SESSIONS/wirbel.desktop" "$SESSIONS/wirbel.desktop.bak-$STAMP" || return 1
    echo "Aus dem Greeter genommen, Backup: $SESSIONS/wirbel.desktop.bak-$STAMP"
  else
    echo "Kein Eintrag da."
  fi
  echo "Die Programmdateien in $LIB und die Config stoeren nicht, sie bleiben."
}

fallback() {
  cat <<T

Rueckfall zu Mango, jederzeit:
  - Im Greeter statt wirbel Mango (dbus) waehlen. Der Greeter merkt sich die letzte Wahl.
  - Haengt wirbel: Strg+Alt+Shift+Ruecktaste beendet es sofort, der Greeter kommt zurueck.
  - Kommt der Greeter nicht: Strg+Alt+F3, einloggen, sudo sv restart greetd.
  - Den Eintrag entfernen: $ROOT/docs/install-session.sh --entfernen
  Das Log steht in ~/.local/state/wirbel, session.log und wirbel.log.
T
}

case ${1:-} in
  --pruefen)
    check
    if [ $FAILED = 0 ]; then echo; echo "Alles da, nichts geaendert."; else echo; echo "Es fehlt etwas, nichts geaendert."; fi
    fallback
    exit $FAILED
    ;;
  --entfernen)
    remove_entry
    exit
    ;;
  --lib-nach)
    [ -n "${2:-}" ] || { echo "Wohin?" >&2; exit 2; }
    make_lib "$2"
    exit
    ;;
  --config-ersetzen) REPLACE=ja ;;
  '') REPLACE=nein ;;
  *) sed -n 2,9p "$0"; exit 2 ;;
esac

check
if [ $FAILED != 0 ]; then
  echo; echo "Es fehlt etwas, nichts eingerichtet."
  exit 1
fi
echo
make_lib "$LIB" && echo "Angelegt: $LIB" || exit 1
place "$ROOT/session/config" "$CONFIG/wirbel/config" "$REPLACE"
place "$ROOT/session/wirbel-portals.conf" "$CONFIG/xdg-desktop-portal/wirbel-portals.conf" ja
install_entry || { echo "Der Greeter-Eintrag ging nicht, Mango bleibt die einzige Wahl."; exit 1; }
echo
echo "Fertig. Beim naechsten Login steht wirbel in der Liste des Greeters, Mango weiter auch."
echo "Wenn wirbel laeuft: wirbel-session --status zeigt im kitty, was da ist."
fallback
