#!/bin/bash
# Führt durch docs/TTY-TEST.md. Auf einem freien TTY starten: ~/WM/docs/tty-run.sh
cd "$HOME/WM" || exit 1
OUT=$HOME/wirbel-test; mkdir -p "$OUT"; R=$OUT/ergebnis.txt
BIN=./target/debug/wirbel; CONF=docs/tty-test.conf
if [ -n "$WAYLAND_DISPLAY$DISPLAY" ] || ! tty | grep -q '^/dev/tty[0-9]'; then
  echo "Bitte auf einem TTY starten, nicht in Mango. Erst Strg+Alt+F3 (oder sudo chvt 3), einloggen, dann nochmal."
  exit 1
fi
echo "=== Lauf $(date) ===" >> "$R"
strat void $BIN check > "$OUT/check.txt" 2>&1; rc=$?
tail -n 1 "$OUT/check.txt"
if [ $rc -ne 0 ] || ! grep -q 'type tty' "$OUT/check.txt"; then
  echo "Vorprüfung fehlgeschlagen. Sag Claude Bescheid, die Ausgabe liegt in $OUT/check.txt"
  echo "check FAIL rc=$rc" >> "$R"; exit 1
fi
cat <<'T'

Notaus, wenn etwas haengt oder schwarz bleibt:
  1. Warten: jeder Test beendet sich selbst.
  2. Strg+Alt+Shift+Ruecktaste
  3. Strg+Alt+F7 geht zu Mango, Strg+Alt+F3 zurueck hierher.
Danach laeuft das Skript von selbst weiter.
T
t() { # Nummer, erwartete Exit-Zahl, Anweisung, Argumente...
  n=$1; want=$2; say=$3; shift 3
  echo; echo "--- Test $n ---"; echo "$say"
  read -r -p "Enter = los, s = ueberspringen, q = Ende: " a
  [ "$a" = q ] && fini
  [ "$a" = s ] && { echo "Test $n uebersprungen" >> "$R"; return; }
  strat void $BIN --config "$CONF" "$@"; got=$?
  stty sane 2>/dev/null
  cp ~/.local/state/wirbel/wirbel.log "$OUT/test$n.log" 2>/dev/null
  if [ "$got" = "$want" ]; then s=ok; else s="FALSCH (erwartet $want)"; fi
  echo "Exit $got: $s"
  read -r -p "Hat alles wie beschrieben ausgesehen? Enter = ja, sonst kurz schreiben was nicht: " note
  echo "Test $n exit $got $s; ${note:-sah gut aus}" >> "$R"
}
fini() {
  echo; echo "Fertig. Ergebnis in $R"
  echo "Kernel-Log sichern (fragt evtl. nach deinem Passwort):"
  sudo dmesg | tail -n 200 > "$OUT/dmesg.txt"
  echo "Jetzt mit Strg+Alt+F7 zurueck zu Mango und Claude sagen, dass du fertig bist."
  exit 0
}
LOCK="sleep 3; qs -p $HOME/WM/docs/tty-lock.qml"
t 1 0 "60 s: Bild mit kitty? Maus bewegen, klicken, tippen, scrollen. Im kitty 'target/debug/wirbel pos' starten: laeuft die Zahl mit? 'target/debug/wirbel move 500 500' testen. Monitor-Menue: 165 Hz?" --timeout 60 -c kitty
t 2 70 "Leerer Bildschirm. Druecke Strg+Alt+Shift+Ruecktaste: sofort zurueck hierher." --timeout 60
t 3 0 "Strg+Alt+F7 zu Mango, kurz warten, Strg+Alt+F3 zurueck: wieder kitty. Endet nach 40 s selbst." --timeout 40 -c kitty
t 4 101 "Nichts tun. Nach 5 s stuerzt es absichtlich ab und du bist wieder hier." --timeout 30 --panic-after 5
t 5 70 "Nach 5 s haengt es absichtlich. Nach 10 s Strg+Alt+Shift+Ruecktaste druecken." --timeout 40 --hang-after 5
t 6 71 "Nichts tun. Haengt absichtlich, nach etwa 15 s von selbst zurueck hierher." --timeout 10 --hang-after 3
t 7 0 "Warten bis 'Gesperrt' erscheint. Strg+Alt+F7, dann Strg+Alt+F3: bleibt gesperrt, nie alter Inhalt. Enter entsperrt. Endet nach 90 s selbst." --timeout 90 -c "$LOCK"
t 8 70 "Warten bis 'Gesperrt' erscheint, dann Strg+Alt+Shift+Ruecktaste." --timeout 90 -c "$LOCK"
t 9 0 "Monitor aus- und wieder einschalten: Bild und kitty kommen zurueck. Endet nach 60 s selbst." --timeout 60 -c kitty
cp "$CONF" "$OUT/nvidia.conf"; echo 'render_gpu = card1' >> "$OUT/nvidia.conf"; CONF=$OUT/nvidia.conf
t 10 0 "Jetzt rendert die NVIDIA. Maus und Scrollen in kitty: fluessig? Endet nach 45 s selbst." --timeout 45 -c kitty
fini
