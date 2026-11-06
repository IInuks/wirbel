#!/bin/bash
# Führt durch docs/TTY-TEST.md. Auf einem freien TTY starten: ~/wirbel/docs/tty-run.sh
# Nur der Nachtest für XWayland, Spiele, Zeiger, Screenshot und Screenshare: ~/wirbel/docs/tty-run.sh nach
# Nur der Abschlusstest, was nach dem Nachtest repariert wurde, und 170 Hz: ~/wirbel/docs/tty-run.sh ende
# Die ganze Sitzung, wie sie der Greeter startet: ~/wirbel/docs/tty-run.sh sitzung
cd "$HOME/wirbel" || exit 1
OUT=$HOME/wirbel-test; mkdir -p "$OUT"; R=$OUT/ergebnis.txt
BIN=./target/release/wirbel; CONF=docs/tty-test.conf
if [ -n "$WAYLAND_DISPLAY$DISPLAY" ] || ! tty | grep -q '^/dev/tty[0-9]'; then
  echo "Bitte auf einem TTY starten, nicht in Mango. Erst Strg+Alt+F3 (oder sudo chvt 3), einloggen, dann nochmal."
  exit 1
fi
if [ ! -x "$BIN" ] || [ -n "$(find src Cargo.toml Cargo.lock -newer "$BIN" -print -quit)" ]; then
  echo "$BIN fehlt oder ist aelter als der Quelltext. Erst in Mango bauen:"
  echo "  cd ~/wirbel && strat -r arch env PATH=\$HOME/.cargo/bin:\$PATH cargo build --release"
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
# Jeder Test bekommt einen eigenen D-Bus ohne Dienste, docs/tty-dbus.conf. Auf dem TTY gibt es sonst
# keinen Bus, und GIO startet dann mit "dbus-launch --autolaunch" selbst einen normalen. Der aktiviert
# ueber /usr/local/share/dbus-1/services das Document-Portal ein zweites Mal, das beendet das der
# Mango-Sitzung und nimmt beim Ende den Mount mit. So ist es in Test 16 passiert: Steam braucht den Bus.
RUN="strat void dbus-run-session --config-file=$HOME/wirbel/docs/tty-dbus.conf --"
PORTALS="$HOME/wirbel/docs/tty-portals.sh $HOME/wirbel/docs/tty-xdpw.conf"
DOC=0; mountpoint -q "$XDG_RUNTIME_DIR/doc" && DOC=1
t() { # Nummer, erwartete Exit-Zahl, Anweisung, Argumente...
  n=$1; want=$2; say=$3; shift 3
  echo; echo "--- Test $n ---"; echo "$say"
  read -r -p "Enter = los, s = ueberspringen, q = Ende: " a
  [ "$a" = q ] && fini
  [ "$a" = s ] && { echo "Test $n uebersprungen" >> "$R"; return; }
  if [ -n "$SESSION" ]; then
    # shellcheck disable=SC2086
    $SESSION --config "$CONF" -- --stats "$@"; got=$?
    cp ~/.local/state/wirbel/session.log "$OUT/test$n-sitzung.log" 2>/dev/null
  else
    $RUN $BIN --stats --config "$CONF" "$@"; got=$?
  fi
  stty sane 2>/dev/null
  cp ~/.local/state/wirbel/wirbel.log "$OUT/test$n.log" 2>/dev/null
  if [ "$DOC" = 1 ] && ! mountpoint -q "$XDG_RUNTIME_DIR/doc"; then
    echo "Test $n: Das Document-Portal der Mango-Sitzung ist weg, bitte Claude sagen." | tee -a "$R"; DOC=0
  fi
  if [ "$got" = "$want" ]; then s=ok; else s="FALSCH (erwartet $want)"; fi
  echo "Exit $got: $s"
  read -r -p "Hat alles wie beschrieben ausgesehen? Enter = ja, sonst kurz schreiben was nicht: " note
  echo "Test $n exit $got $s; ${note:-sah gut aus}" >> "$R"
}
fini() {
  grep -hE 'direct scanout|Xwayland (died|went)|no dmabuf|screen capture' "$OUT"/test[12][0-9].log >> "$R" 2>/dev/null
  echo; echo "Fertig. Ergebnis in $R"
  echo "Kernel-Log sichern (fragt evtl. nach deinem Passwort):"
  sudo dmesg | tail -n 200 > "$OUT/dmesg.txt"
  echo "Jetzt mit Strg+Alt+F7 zurueck zu Mango und Claude sagen, dass du fertig bist."
  exit 0
}
if [ "$1" = ende ]; then
  # Abschlusstest. Prueft, was Session 08 repariert oder gefunden hat, und den Alltags-Mode 170 Hz.
  CHROMIUM="chromium --user-data-dir=$OUT/chromium --no-first-run --no-default-browser-check --password-store=basic"
  PAGE="file://$HOME/wirbel/docs/pointer-test.html"
  CONF=docs/tty-170.conf
  t 22 0 "170 Hz: Bild mit kitty? Das Monitor-Menue muss 170 Hz zeigen, nicht 165. Maus bewegen, im kitty scrollen: fluessig, kein Flackern, kein Schwarzbild? Endet nach 40 s selbst." --timeout 40 -c kitty
  grep -a 'output added' "$OUT/test22.log" | sed 's/.*name=/170-Hz-Test: name=/' >> "$R"
  sed -i 's/"exit_type":"[A-Za-z]*"/"exit_type":"Crashed"/; s/"exited_cleanly":true/"exited_cleanly":false/' "$OUT/chromium/Default/Preferences" 2>/dev/null
  t 23 0 "Blase in Chromium. Du siehst Chromium mit dem Hinweis 'Restore pages?' oben rechts im Fenster. Druecke Super+Return: Ein kitty teilt den Schirm rechts. Die Blase wandert mit und bleibt oben rechts IM Chromium-Fenster, hinter dem kitty ist nichts mehr von ihr zu sehen. Klicke auf das X der Blase: Sie geht weg. Endet nach 60 s selbst." --timeout 60 -c "$CHROMIUM --ozone-platform=wayland $PAGE"
  t 24 0 "Scrollen messen. Scrolle 30 Sekunden mit dem Mausrad in dieser langen Seite, wie du es sonst auch tust, schnell und langsam. Stockt es? Wie oft ungefaehr? Das Log schreibt mit. Endet nach 45 s selbst." --timeout 45 -c "$CHROMIUM --ozone-platform=wayland file://$HOME/wirbel/docs/scroll-test.html"
  t 25 0 "Scrollen in Steam. Bibliothek oder Shop oeffnen und mit dem Mausrad scrollen wie vorhin in Test 16. Stockt es? Notiere es. Starte danach ein Spiel und schau auf die FPS. Endet nach 3 Minuten selbst, Strg+Alt+Shift+Ruecktaste bricht ab (dann steht hier 70, das ist ok)." --timeout 180 -c steam
  cp "$CONF" "$OUT/nvidia.conf"; echo 'render_gpu = card1' >> "$OUT/nvidia.conf"; CONF=$OUT/nvidia.conf
  t 26 0 "Die NVIDIA rendert, der Client hat ihren Treiber aus Void: Zahnraeder im Vollbild? Dass sie gross sind und oben und unten abgeschnitten, ist bei glxgears normal. Endet nach 25 s selbst." --timeout 25 -c "__NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia strat void /bedrock/strata/debian/usr/bin/glxgears -fullscreen"
  CONF=docs/tty-test.conf
  grep -ahE ' stats ' "$OUT"/test2[2-6].log | sed 's/^[^ ]* *[A-Z]* *[^ ]* //' > "$OUT/stats.txt"
  echo "Messwerte: $OUT/stats.txt" >> "$R"
  fini
fi
if [ "$1" = sitzung ]; then
  # Die Sitzung startet alles selbst, wie im Greeter: Bus, PipeWire, Portale, Autostart aus der Vorlage.
  # Laeuft Mango noch, ist sie Gast: Ein Bus ohne Dienste, Mangos PipeWire, und in der Config drei
  # Aenderungen, sonst blieben Bar und Hintergrund bei den Instanzen von Mango. Mit ausgeloggtem Mango
  # laeuft alles wie im Alltag, das zeigt aber auch der Test ueber den Greeter, docs/SESSION.md.
  docs/install-session.sh --lib-nach "$OUT/lib" || exit 1
  SESSION=$OUT/lib/wirbel-session
  CONF=$OUT/sitzung.conf
  if mountpoint -q "$XDG_RUNTIME_DIR/doc" || pgrep -u "$(id -u)" -x mango >/dev/null; then
    SESSION="$SESSION --gast"
    echo "Mango laeuft, die Sitzung ist Gast. Tastenbund und Portale sind dieselben."
    sed -E 's/ -n$//; /wallsync|launcher-host|widgets-host/d; s|^exec-once = \$HOME/.local/bin/wall-restore|exec-once = awww-daemon \& sleep 1; $HOME/.local/bin/wall-restore|' session/config > "$CONF"
  else
    cp session/config "$CONF"
  fi
  CHECK="sleep 10; wirbel-session --status > $OUT/status.txt 2>&1"
  t 30 0 "Die Sitzung. Du siehst nach einigen Sekunden dein Wallpaper, oben die salbei-Bar mit den Workspaces 1 2 3 und irgendwo den Blob. Dann: Super+Return oeffnet kitty, in der Bar ist Workspace 1 jetzt belegt. Super+2: Die Bar springt auf 2, das Fenster ist weg. Klicke in der Bar auf die 1: kitty ist wieder da. Super+Leertaste oeffnet den Launcher, Esc schliesst ihn. Super+Esc oeffnet das Power-Menue, Esc schliesst es. Nichts auswaehlen. Endet nach 2 Minuten selbst." --timeout 120 -c "$CHECK"
  cat "$OUT/status.txt" >> "$R" 2>/dev/null
  t 31 0 "Tasten aus Mango. Erst Super+Return, zweimal, dann Super+Shift+3, damit ein kitty auf Workspace 3 liegt, und Super+1. 1. Super halten und Mausrad runter: Sprung auf 3, die leeren werden uebersprungen, rauf: zurueck auf 1. Im kitty scrollt dabei nichts. 2. Super+Rechts HALTEN: Nach 0,6 s laufen die Workspaces in der Bar schnell durch, loslassen stoppt. Super+1. 3. Strg halten, dann RECHTES Shift: Das quickdots-Menue kommt, Esc schliesst es. 4. Im kitty tippen: AltGr+Q ergibt @, AltGr+7 ergibt {, AltGr+E ergibt das Euro-Zeichen. Das kitty bleibt dabei offen. 5. Strg+Alt+Rechts halten: Die Fenster aendern laufend ihre Breite. Endet nach 2 Minuten selbst." --timeout 120
  t 32 0 "Abmelden ueber die Bar: Super+Esc, dann Logout. Die Sitzung endet sofort und du bist hier zurueck, mit Exit 0. Endet sonst nach 60 s selbst." --timeout 60
  rm -f "$OUT"/shot-*.png
  t 33 0 "Screenshot. Super+Shift+S: Flameshot, der Schirm wird dunkler. Rechteck ziehen, Strg+C: Das Dunkle verschwindet. Dann Super+Return, im kitty 'grim $OUT/shot-sitzung.png' eingeben, die Datei muss danach dort liegen. Endet nach 60 s selbst." --timeout 60
  ls -l "$OUT"/shot-sitzung.png >> "$R" 2>&1
  rm -rf "$OUT/chromium-share"
  SHARE="chromium --user-data-dir=$OUT/chromium-share --no-first-run --no-default-browser-check --password-store=basic --ozone-platform=wayland file://$HOME/wirbel/docs/share-test.html"
  t 34 0 "Bildschirm teilen, diesmal mit den Portalen der Sitzung und ohne Extra-Schalter fuer Chromium. Auf 'Teilen starten' klicken, Reiter 'Entire Screen', Vorschaubild, 'Share'. Danach zeigt die schwarze Flaeche deinen Schirm wie ein Tunnel, aufrecht, und die Bildzahl oben zaehlt hoch, solange sich etwas bewegt. Endet nach 90 s selbst." --timeout 90 -c "$SHARE"
  echo "Wenn alles passt: docs/install-session.sh --pruefen, dann ohne --pruefen, und ueber den Greeter einloggen, docs/SESSION.md."
  fini
fi
if [ "$1" != nach ]; then
LOCK="sleep 3; qs -p $HOME/wirbel/docs/tty-lock.qml"
t 1 0 "60 s: Bild mit kitty? Maus bewegen, klicken, tippen, scrollen. Im kitty 'target/release/wirbel pos' starten: laeuft die Zahl mit? 'target/release/wirbel move 500 500' testen. Monitor-Menue: 165 Hz?" --timeout 60 -c kitty
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
PAGE="file://$HOME/wirbel/docs/pointer-test.html"
t 11 0 "X11: kitty und drehende Zahnraeder nebeneinander? Klick in beide Fenster, im kitty tippen. Super+W schliesst die Zahnraeder. Endet nach 40 s selbst." --timeout 40 -c kitty -c glxgears
t 12 0 "Chromium ueber X11. Seite klicken: Zeiger weg, Zahlen laufen weiter, auch wenn du die Maus weit schiebst? Esc: Zeiger wieder da? Rechtsklick: Menue kommt und geht? Text aus der Seite kopieren und im kitty einfuegen, und umgekehrt. Endet nach 100 s selbst." --timeout 100 -c kitty -c "$CHROMIUM --ozone-platform=x11 $PAGE"
t 13 0 "Dasselbe als Wayland-Client: Seite klicken, Zahlen, Esc. Endet nach 60 s selbst." --timeout 60 -c "$CHROMIUM --ozone-platform=wayland $PAGE"
t 14 0 "Vollbild: Zahnraeder ueber den ganzen Schirm, gross und oben und unten abgeschnitten, das ist bei glxgears in einem breiten Fenster normal. Zeiger sichtbar, keine Streifen oder Flackern? Endet nach 25 s selbst." --timeout 25 -c "glxgears -fullscreen"
CONF=$OUT/nvidia.conf
t 15 0 "Vollbild, die NVIDIA rendert, der Client nimmt ihren Treiber aus Void: Bild ok? Endet nach 25 s selbst." --timeout 25 -c "__NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia strat void /bedrock/strata/debian/usr/bin/glxgears -fullscreen"
CONF=docs/tty-test.conf
t 16 0 "Steam: Startet es, gehen Menues und Einstellungen? Starte ein Spiel mit Mausblick: Blick dreht sich endlos, Fenster ist Vollbild, Alt+Tab-Ersatz ueber deine Binds geht. Notiere die FPS. Endet nach 5 Minuten selbst, Strg+Alt+Shift+Ruecktaste bricht ab (dann steht hier 70, das ist ok)." --timeout 300 -c steam
t 17 0 "Offen aus Test 1: Im kitty 'target/release/wirbel pos' starten, Maus bewegen: Zahl laeuft. Strg+C: Befehl endet, Fenster bleibt offen. Dann 'target/release/wirbel move 500 500': Zeiger springt. Endet nach 60 s selbst." --timeout 60 -c kitty

# Screenshots und Screenshare aus Session 07. Nested ist das Bild gespiegelt und der Schirm klein,
# hier zaehlt der echte Bildschirm. Die Bilder landen in $OUT, Claude schaut sie sich an.
rm -f "$OUT"/shot-*.png
[ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || echo "PipeWire laeuft nicht. Es kommt aus der Mango-Sitzung auf tty7. Ohne sie gehen die Tests 20 und 21 nicht."
SHOT="sleep 4; grim $OUT/shot-grim.png; grim -c -g '100,100 800x600' $OUT/shot-grim-teil.png; kitty --hold sh -c 'ls -l $OUT/shot-grim*.png; echo; echo Fertig wenn hier ZWEI Zeilen stehen'"
t 18 0 "Screenshot mit grim, von selbst, du tippst nichts. Du siehst: Zuerst fuellt ein kitty mit einer Eingabezeile den ganzen Schirm. Nach 4 Sekunden teilt sich der Schirm, rechts geht ein ZWEITES kitty auf, darin stehen zwei Zeilen mit shot-grim.png und shot-grim-teil.png und darunter 'Fertig wenn hier ZWEI Zeilen stehen'. Mehr passiert nicht. Endet nach 12 s selbst." --timeout 12 -c kitty -c "$SHOT"
ls -l "$OUT"/shot-grim*.png >> "$R" 2>&1
t 19 0 "Flameshot. Du siehst: Ein kitty fuellt den Schirm, darin steht ein Text. Nach 3 Sekunden wird der ganze Schirm dunkler, mittig eine Hilfe-Box in Lila, links ein schmaler lila Reiter 'Tool Settings'. Ziehe mit gedrueckter linker Maustaste ein Rechteck: Es bleibt hell, drumherum dunkel, am Rand erscheinen runde Knoepfe. Druecke Strg+C: Das Dunkle verschwindet, du siehst wieder das kitty. Endet nach 45 s selbst, auch wenn du nichts tust." --timeout 45 -c "$PORTALS" -c "kitty sh -c 'echo Dieser Text kommt im Screenshot vor; sleep 60'" -c "sleep 2; XDG_CURRENT_DESKTOP=wlroots QT_QPA_PLATFORM=wayland flameshot gui"
rm -rf "$OUT/chromium-share"
SHARE="chromium --user-data-dir=$OUT/chromium-share --no-first-run --no-default-browser-check --password-store=basic --ozone-platform=wayland --enable-features=WebRTCPipeWireCapturer file://$HOME/wirbel/docs/share-test.html"
t 20 0 "Screenshare wie bei Discord. Du siehst: Chromium mit dunkelblauer Seite, oben links der Knopf 'Teilen starten', daneben 'Noch nichts geteilt.', darunter eine schwarze Flaeche. Mach das: 1. 'Teilen starten' klicken: Ein weisser Dialog 'Choose what to share' kommt. 2. Oben im Dialog den Reiter 'Entire Screen' klicken: Ein kleines Vorschaubild deines Schirms erscheint, Text 'Entire screen'. 3. Das Vorschaubild anklicken, dann unten rechts 'Share'. Danach: Die schwarze Flaeche zeigt deinen Schirm, darin wieder die Seite, immer kleiner ineinander wie ein Tunnel, AUFRECHT, nicht auf dem Kopf. Oben steht jetzt '2560 x 1440, Bild N', und N zaehlt hoch, solange sich etwas bewegt. Eine kleine Leiste 'is sharing your screen' mit 'Stop sharing' schwebt mittig, das ist normal. Bewege die Maus im Bild: Laeuft sie auch im Tunnel mit? Ruckelt deine echte Maus dabei? Notiere das. Endet nach 90 s selbst." --timeout 90 -c "$PORTALS" -c "sleep 2; XDG_SESSION_TYPE=wayland exec $SHARE"
t 21 0 "Screenshare in Discord. Laeuft Discord noch in Mango, beende es dort vorher ganz, sonst kommt hier kein Fenster. Du siehst: Discord oeffnet sich, das dauert etwas. Gehe in einen Sprachkanal, klicke auf 'Bildschirm teilen', waehle deinen Bildschirm, starte den Stream. Ergebnis: In Discord erscheint die Vorschau deines Streams, und sie bewegt sich mit. Wenn du es nicht ausprobieren magst: s fuer ueberspringen. Endet nach 4 Minuten selbst, Strg+Alt+Shift+Ruecktaste bricht ab (dann steht hier 70, das ist ok)." --timeout 240 -c "$PORTALS" -c "sleep 2; XDG_SESSION_TYPE=wayland exec $HOME/.local/bin/discord --enable-features=WebRTCPipeWireCapturer --ozone-platform=wayland"
fini
