# wirbel auf einem freien TTY testen

Am einfachsten: `~/WM/docs/tty-run.sh` auf dem TTY starten, das Skript führt durch alle Tests unten und schreibt das Ergebnis nach `~/wirbel-test/ergebnis.txt`. Gestartet wird der Release-Build, das Skript lehnt einen veralteten ab. Jeder Test läuft mit `--stats` auf einem eigenen D-Bus ohne Dienste.

Dauert etwa 15 Minuten. Mango läuft auf tty7 weiter, mit **Strg+Alt+F7** kommst du jederzeit zurück.
Der Monitor hängt an der NVIDIA-Karte, deren Treiber-Bibliotheken liegen nur im Void-Stratum. Darum
wird in Arch gebaut, aber mit `strat void` gestartet.

## Notausgänge, zuerst lesen

Der Reihe nach, wenn etwas nicht reagiert oder der Bildschirm schwarz bleibt:

1. **Warten.** Jeder Befehl unten hat `--timeout`, wirbel beendet sich dann selbst, spätestens 5
   Sekunden danach mit Gewalt.
2. **Strg+Alt+Shift+Rücktaste** beendet wirbel sofort, auch gesperrt und bei hängender Hauptschleife.
3. **Strg+Alt+F7** wechselt zu Mango, **Strg+Alt+F3** zurück. Geht nicht, wenn wirbel hängt.
4. **Alt+Druck+R**, dann **Alt+Druck+K** (SysRq): holt die Tastatur zurück und beendet alles auf
   diesem TTY. Danach Strg+Alt+F7.
5. Letzter Ausweg: **Alt+Druck+S**, **U**, **B** nacheinander: Dateien sichern und Neustart.

Nach einem Schwarzbild erst einige Sekunden warten, denn der Mode-Wechsel des Monitors kann dauern.
Bleibt der TTY nach dem Beenden schwarz, blind `reset` und Enter tippen oder auf ein anderes TTY
wechseln (Strg+Alt+F4). Danach die Logs mitbringen, siehe unten.

## Vorbereitung, in Mango

```sh
cd ~/WM
strat -r arch env PATH=$HOME/.cargo/bin:$PATH cargo build --release
strat void ./target/release/wirbel check
```

Am Ende muss `No problems found.` stehen. Dann **Strg+Alt+F3** und einloggen. Passiert nichts, in
einem Terminal `sudo chvt 3` nutzen: Deine Mango-Config belegt die Tasten nicht selbst. Zurück zu Mango
geht vom TTY mit Strg+Alt+F7 oder `sudo chvt 7`. Auf dem TTY:

```sh
cd ~/WM
alias w='strat void ./target/release/wirbel --config docs/tty-test.conf'
strat void ./target/release/wirbel check
```

Bei `Session` muss jetzt `type tty` und `active yes` stehen, keine Zeile mit `FAIL`. Stimmt das nicht:
hier aufhören und die Ausgabe mitbringen.

## Die Tests

Nach jedem Test `echo $?` und die Zahl mit der Tabelle vergleichen. Danach muss der TTY wieder ganz
normal Text anzeigen und Eingaben annehmen.

| Nr | Befehl | Was du tust | Das soll passieren | Exit |
|----|--------|-------------|--------------------|------|
| 1 | `w --timeout 15 -c kitty` | Maus bewegen, tippen | Bild mit kitty, Zeiger sichtbar, Monitor-Menü zeigt 165 Hz, nach 15 s zurück am TTY | 0 |
| 2 | `w --timeout 60` | Strg+Alt+Shift+Rücktaste | Sofort zurück am TTY | 70 |
| 3 | `w --timeout 40 -c kitty` | Strg+Alt+F7, in Mango kurz warten, Strg+Alt+F3 | Mango ist unversehrt, danach wieder wirbel mit kitty | 0 |
| 4 | `w --timeout 30 --panic-after 5` | nichts | Nach 5 s Meldung `panic` auf dem TTY | 101 |
| 5 | `w --timeout 40 --hang-after 5` | Nach 10 s Strg+Alt+Shift+Rücktaste | wirbel reagiert ab 5 s nicht mehr, die Taste beendet es trotzdem | 70 |
| 6 | `w --timeout 10 --hang-after 3` | nichts | Nach 15 s von selbst zurück am TTY | 71 |
| 7 | `w --timeout 90 -c 'sleep 3; qs -p ~/WM/docs/tty-lock.qml'` | Warten, bis `Gesperrt` kommt. Strg+Alt+F7 und zurück mit Strg+Alt+F3: bleibt gesperrt. Enter entsperrt | Nur die Sperre sichtbar, nie alter Inhalt | 0 |
| 8 | wie Nr 7 | Während `Gesperrt` steht: Strg+Alt+Shift+Rücktaste | Sofort zurück am TTY | 70 |
| 9 | `w --timeout 60 -c kitty` | Monitor aus- und einschalten oder Kabel ziehen und stecken | Bild kommt wieder, kitty lebt, auch wenn der Ausgang von DP-4 zu DP-3 wechselt | 0 |

Zum Schluss die zweite GPU: Bisher rendert die AMD-iGPU und die Bilder werden zur NVIDIA kopiert, wie
bei Mango. Jetzt rendert die NVIDIA selbst:

```sh
cp docs/tty-test.conf /tmp/wirbel-nvidia.conf
echo 'render_gpu = card1' >> /tmp/wirbel-nvidia.conf
strat void ./target/release/wirbel --config /tmp/wirbel-nvidia.conf --timeout 20 -c kitty
```

Erwartet: Bild mit kitty, Exit 0. Fühlt sich etwas ruckelig an (Zeiger, Scrollen in kitty), bitte
notieren, und bei welcher GPU.

## Nachtest nach Etappe 6 und 7

`~/WM/docs/tty-run.sh nach` startet nur die Tests 11 bis 21 und schreibt nach
`~/wirbel-test/ergebnis.txt`, dazu die wichtigen Log-Zeilen. Die Tests 1 bis 10 sind bestanden und
müssen nicht wiederholt werden. Geprüft wird, was nested nicht geht: X11-Programme auf dem echten
Schirm, Zeiger-Sperre und relative Bewegung mit der echten Maus (`docs/pointer-test.html`), Vollbild,
Steam mit einem Spiel, `wirbel pos` mit Strg+C und `wirbel move`. Dazu Screenshots mit grim (Test 18) und
flameshot (19), Bildschirm teilen in Chromium (20) und Discord (21). Für 20 und 21 muss Mango auf tty7 laufen,
PipeWire kommt von dort, und Discord muss dort beendet sein. Die Portale laufen in einem eigenen D-Bus
(`docs/tty-dbus.conf`) und berühren die von Mango nicht. Vor dem Test einmal bauen.

## Abschlusstest

`~/WM/docs/tty-run.sh ende` startet nur die Tests 22 bis 26, etwa 8 Minuten, Mango muss nicht beendet werden. Geprüft
wird, was nach dem Nachtest repariert oder gefunden wurde: 170 Hz als Alltags-Mode (22), die Blase von Chromium,
wenn ein zweites Fenster dazukommt (23), Scrollen in einer langen Seite (24) und in Steam (25), jeweils mit
Messwerten, dazu die NVIDIA als Render-GPU mit Voids Treiber im Client (26). Die Messwerte landen in
`~/wirbel-test/stats.txt`, die Bedeutung steht oben in `src/stats.rs`. Bei `input` und `late` zeigt sich, ob
wirbel selbst stockt, bei `scroll`, ob es der Client ist.

## Die ganze Sitzung

`~/WM/docs/tty-run.sh sitzung` startet die Tests 30 bis 34, etwa 10 Minuten. Hier läuft `wirbel-session` statt wirbel allein,
also mit Bus, Portalen, Autostart und der Config-Vorlage: Bar und Wallpaper (30), die Tasten aus Mango (31), Logout über
die Bar (32), Screenshot (33), Bildschirm teilen über die Portale der Sitzung (34). Mango darf laufen, die Sitzung ist dann
Gast, siehe `docs/SESSION.md`. Was nur ohne Mango geht, zeigt der Login über den Greeter, ebenfalls dort beschrieben.

## Was du mitbringen sollst

Nach einem Fehler sofort, denn der nächste Start verschiebt das Log nach `wirbel.log.old`:

```sh
mkdir -p ~/wirbel-test
cp ~/.local/state/wirbel/wirbel.log* ~/wirbel-test/
strat void ./target/release/wirbel check > ~/wirbel-test/check.txt 2>&1
sudo dmesg | tail -n 200 > ~/wirbel-test/dmesg.txt
```

Dazu: Nummer des Tests, Exit-Zahl, was auf dem Bildschirm zu sehen war. Wichtige Zeilen im Log sind
`got a session`, `output added` und `first frame is on the screen`: Fehlt die letzte, kam nie ein Bild
beim Monitor an. Für mehr Details den Fehler wiederholen, mit `RUST_LOG=wirbel=debug,smithay=info w ...`.
