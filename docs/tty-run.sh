#!/bin/bash
# Führt durch docs/TTY-TEST.md. Auf einem freien TTY starten: ~/WM/docs/tty-run.sh
# Nur der Nachtest für XWayland, Spiele, Zeiger, Screenshot und Screenshare: ~/WM/docs/tty-run.sh nach
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
RUN="strat void"
# Test 18 und mehr: mit eigenem D-Bus, sonst sehen die Portale nichts. Siehe docs/tty-dbus.conf.
RUNBUS="strat void dbus-run-session --config-file=$HOME/WM/docs/tty-dbus.conf --"
PORTALS="$HOME/WM/docs/tty-portals.sh $HOME/WM/docs/tty-xdpw.conf"
t() { # Nummer, erwartete Exit-Zahl, Anweisung, Argumente...
  n=$1; want=$2; say=$3; shift 3
  echo; echo "--- Test $n ---"; echo "$say"
  read -r -p "Enter = los, s = ueberspringen, q = Ende: " a
  [ "$a" = q ] && fini
  [ "$a" = s ] && { echo "Test $n uebersprungen" >> "$R"; return; }
  $RUN $BIN --config "$CONF" "$@"; got=$?
  stty sane 2>/dev/null
  cp ~/.local/state/wirbel/wirbel.log "$OUT/test$n.log" 2>/dev/null
  if [ "$got" = "$want" ]; then s=ok; else s="FALSCH (erwartet $want)"; fi
  echo "Exit $got: $s"
  read -r -p "Hat alles wie beschrieben ausgesehen? Enter = ja, sonst kurz schreiben was nicht: " note
  echo "Test $n exit $got $s; ${note:-sah gut aus}" >> "$R"
}
fini() {
  grep -hE 'direct scanout|explicit sync|Xwayland (is ready|died|went)|no dmabuf|screen capture' "$OUT"/test[12][0-9].log >> "$R" 2>/dev/null
  echo; echo "Fertig. Ergebnis in $R"
  echo "Kernel-Log sichern (fragt evtl. nach deinem Passwort):"
  sudo dmesg | tail -n 200 > "$OUT/dmesg.txt"
  echo "Jetzt mit Strg+Alt+F7 zurueck zu Mango und Claude sagen, dass du fertig bist."
  exit 0
}
if [ "$1" != nach ]; then
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
CONF=docs/tty-test.conf
fi

# Nachtest aus Session 06. Braucht die echte Maus, ein Spiel und den echten Bildschirm.
cp "$CONF" "$OUT/nvidia.conf"; echo 'render_gpu = card1' >> "$OUT/nvidia.conf"
CHROMIUM="chromium --user-data-dir=$OUT/chromium --no-first-run --no-default-browser-check --password-store=basic"
PAGE="file://$HOME/WM/docs/pointer-test.html"
t 11 0 "X11: kitty und drehende Zahnraeder nebeneinander? Klick in beide Fenster, im kitty tippen. Super+W schliesst die Zahnraeder. Endet nach 40 s selbst." --timeout 40 -c kitty -c glxgears
t 12 0 "Chromium ueber X11. Seite klicken: Zeiger weg, Zahlen laufen weiter, auch wenn du die Maus weit schiebst? Esc: Zeiger wieder da? Rechtsklick: Menue kommt und geht? Text aus der Seite kopieren und im kitty einfuegen, und umgekehrt. Endet nach 100 s selbst." --timeout 100 -c kitty -c "$CHROMIUM --ozone-platform=x11 $PAGE"
t 13 0 "Dasselbe als Wayland-Client: Seite klicken, Zahlen, Esc. Endet nach 60 s selbst." --timeout 60 -c "$CHROMIUM --ozone-platform=wayland $PAGE"
t 14 0 "Vollbild: Zahnraeder ueber den ganzen Schirm, Zeiger sichtbar, keine Streifen oder Flackern? Endet nach 25 s selbst." --timeout 25 -c "glxgears -fullscreen"
CONF=$OUT/nvidia.conf
t 15 0 "Vollbild, die NVIDIA rendert: Bild ok? Langsam ist hier normal, die Clients haben keine NVIDIA-Treiber. Endet nach 25 s selbst." --timeout 25 -c "glxgears -fullscreen"
CONF=docs/tty-test.conf
t 16 0 "Steam: Startet es, gehen Menues und Einstellungen? Starte ein Spiel mit Mausblick: Blick dreht sich endlos, Fenster ist Vollbild, Alt+Tab-Ersatz ueber deine Binds geht. Notiere die FPS. Endet nach 5 Minuten selbst, Strg+Alt+Shift+Ruecktaste bricht ab (dann steht hier 70, das ist ok)." --timeout 300 -c steam
t 17 0 "Offen aus Test 1: Im kitty 'target/debug/wirbel pos' starten, Maus bewegen: Zahl laeuft. Strg+C: Befehl endet, Fenster bleibt offen. Dann 'target/debug/wirbel move 500 500': Zeiger springt. Endet nach 60 s selbst." --timeout 60 -c kitty

# Screenshots und Screenshare aus Session 07. Nested ist das Bild gespiegelt und der Schirm klein,
# hier zaehlt der echte Bildschirm. Die Bilder landen in $OUT, Claude schaut sie sich an.
rm -f "$OUT"/shot-*.png
[ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || echo "PipeWire laeuft nicht. Es kommt aus der Mango-Sitzung auf tty7. Ohne sie gehen die Tests 20 und 21 nicht."
SHOT="sleep 4; grim $OUT/shot-grim.png; grim -c -g '100,100 800x600' $OUT/shot-grim-teil.png; kitty --hold sh -c 'ls -l $OUT/shot-grim*.png; echo; echo Fertig wenn hier ZWEI Zeilen stehen'"
t 18 0 "Screenshot mit grim, von selbst, du tippst nichts. Du siehst: Zuerst nur links ein kitty mit einer Eingabezeile. Nach 4 Sekunden geht rechts ein ZWEITES kitty auf, darin stehen zwei Zeilen mit shot-grim.png und shot-grim-teil.png und darunter 'Fertig wenn hier ZWEI Zeilen stehen'. Mehr passiert nicht. Endet nach 12 s selbst." --timeout 12 -c kitty -c "$SHOT"
ls -l "$OUT"/shot-grim*.png >> "$R" 2>&1
RUN=$RUNBUS
t 19 0 "Flameshot. Du siehst: Ein kitty fuellt den Schirm, darin steht ein Text. Nach 3 Sekunden wird der ganze Schirm dunkler, mittig eine Hilfe-Box in Lila, links ein schmaler lila Reiter 'Tool Settings'. Ziehe mit gedrueckter linker Maustaste ein Rechteck: Es bleibt hell, drumherum dunkel, am Rand erscheinen runde Knoepfe. Druecke Strg+C: Das Dunkle verschwindet, du siehst wieder das kitty. Endet nach 45 s selbst, auch wenn du nichts tust." --timeout 45 -c "$PORTALS" -c "kitty sh -c 'echo Dieser Text kommt im Screenshot vor; sleep 60'" -c "sleep 2; XDG_CURRENT_DESKTOP=wlroots QT_QPA_PLATFORM=wayland flameshot gui"
SHARE="chromium --user-data-dir=$OUT/chromium --no-first-run --no-default-browser-check --password-store=basic --ozone-platform=wayland --enable-features=WebRTCPipeWireCapturer file://$HOME/WM/docs/share-test.html"
t 20 0 "Screenshare wie bei Discord. Du siehst: Chromium mit dunkelblauer Seite, oben links der Knopf 'Teilen starten', daneben 'Noch nichts geteilt.', darunter eine schwarze Flaeche. Mach das: 1. 'Teilen starten' klicken: Ein weisser Dialog 'Choose what to share' kommt. 2. Oben im Dialog den Reiter 'Entire Screen' klicken: Ein kleines Vorschaubild deines Schirms erscheint, Text 'Entire screen'. 3. Das Vorschaubild anklicken, dann unten rechts 'Share'. Danach: Die schwarze Flaeche zeigt deinen Schirm, darin wieder die Seite, immer kleiner ineinander wie ein Tunnel, AUFRECHT, nicht auf dem Kopf. Oben steht jetzt '2560 x 1440, Bild N', und N zaehlt hoch, solange sich etwas bewegt. Eine kleine Leiste 'is sharing your screen' mit 'Stop sharing' schwebt mittig, das ist normal. Bewege die Maus im Bild: Laeuft sie auch im Tunnel mit? Ruckelt deine echte Maus dabei? Notiere das. Endet nach 90 s selbst." --timeout 90 -c "$PORTALS" -c "sleep 2; XDG_SESSION_TYPE=wayland exec $SHARE"
t 21 0 "Screenshare in Discord. Laeuft Discord noch in Mango, beende es dort vorher ganz, sonst kommt hier kein Fenster. Du siehst: Discord oeffnet sich, das dauert etwas. Gehe in einen Sprachkanal, klicke auf 'Bildschirm teilen', waehle deinen Bildschirm, starte den Stream. Ergebnis: In Discord erscheint die Vorschau deines Streams, und sie bewegt sich mit. Wenn du es nicht ausprobieren magst: s fuer ueberspringen. Endet nach 4 Minuten selbst, Strg+Alt+Shift+Ruecktaste bricht ab (dann steht hier 70, das ist ok)." --timeout 240 -c "$PORTALS" -c "sleep 2; XDG_SESSION_TYPE=wayland exec $HOME/.local/bin/discord --enable-features=WebRTCPipeWireCapturer --ozone-platform=wayland"
RUN="strat void"
fini
