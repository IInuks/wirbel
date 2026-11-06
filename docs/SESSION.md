# wirbel als Alltagssitzung

## Einrichten
Alles Systemweite macht `docs/install-session.sh`, gestartet vom Nutzer, nach `cargo build --release`.
1. `docs/install-session.sh --pruefen` ändert nichts und sagt, was fehlt und was passieren würde.
2. `docs/install-session.sh` legt an:
   - `~/.local/lib/wirbel/`: `wirbel`, `wirbel-session`, `mmsg` (Symlink), `bus.conf`, `xdpw.conf`. Eine Kopie, kein Link
     auf `target/`: `cargo build` tauscht nichts unter einer laufenden Sitzung aus. Nach einem neuen Build das Skript
     noch einmal starten.
   - `~/.config/wirbel/config`, aus `session/config`. Gibt es sie schon, bleibt sie, die Vorlage liegt als
     `config.vorlage` daneben. `--config-ersetzen` ersetzt sie mit Backup.
   - `~/.config/xdg-desktop-portal/wirbel-portals.conf`. Die `portals.conf` daneben bleibt, wie sie ist.
   - `/bedrock/strata/arch/usr/share/wayland-sessions/wirbel.desktop`, per sudo. Der Greeter läuft in Arch und liest
     dieses Verzeichnis.

Mango, `~/.config/mango`, greetd und quickdots bleiben unberührt.

## Rückfall zu Mango
- Im Greeter Mango wählen, der Eintrag bleibt. Der Greeter merkt sich die letzte Wahl, nach wirbel steht also wirbel
  vorn.
- wirbel hängt: Strg+Alt+Shift+Rücktaste beendet es, der Greeter kommt zurück. Startet wirbel gar nicht, endet die
  Sitzung und der Greeter kommt von selbst.
- Greeter kommt nicht: Strg+Alt+F3, einloggen, `sudo sv restart greetd`.
- `docs/install-session.sh --entfernen` nimmt den Eintrag aus der Liste, mit Backup.
- Logs in `~/.local/state/wirbel`, jeweils mit `.old`: `session.log` (Sitzung, PipeWire), `wirbel.log` (wirbel und alle
  Programme, die es gestartet hat, auch die Portale).

## Testen
1. `docs/tty-run.sh sitzung` auf einem TTY, Tests 30 bis 34, Mango darf dabei laufen.
2. Einrichten, in Mango ausloggen, im Greeter wirbel wählen. Wallpaper und Bar kommen wie in Test 30.
3. Im kitty `wirbel-session --status`. Erwartet: `guest: no`, jeder Eintrag `is there` und `has a backend`, nur
   `org.freedesktop.secrets` kommt erst, wenn ein Programm den Schlüsselbund braucht.
4. Ton, Discord (ist es noch angemeldet? Das braucht den Schlüsselbund), Passwörter in Chromium, Polkit-Dialog,
   Steam mit einem Spiel, Logout über die Bar.

## Was wirbel-session tut
1. Wechselt nach Void, falls es woanders gestartet wurde: Dort liegen die NVIDIA-Bibliotheken.
2. Setzt `XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP=wirbel:wlroots`, `XDG_SESSION_DESKTOP` und setzt `~/.local/lib/wirbel`
   vorn in den `PATH`, das macht den `mmsg`-Ersatz sichtbar.
3. Startet den Bus selbst, nicht mit `dbus-run-session`: Das stirbt an einem TERM und lässt alles darunter weiterlaufen.
4. Startet PipeWire, WirePlumber und pipewire-pulse aus Debian, wie Mango, mit Neustart. Läuft schon ein PipeWire, bleibt
   das.
5. Startet wirbel, das als erstes Kind die Portale startet: Sie brauchen den Compositor. Danach laufen die
   `exec-once`-Zeilen der Config.
6. Nach dem Ende von wirbel beendet es, was es gestartet hat, über die PIDs. Kein `pkill`.

**Zwei Busse.** Allein ist es ein normaler Bus wie unter Mango. Schlüsselbund, dconf, gvfs und das Document-Portal starten
über ihn, ohne sie wären zum Beispiel gespeicherte Anmeldungen weg. Er ist der einzige des Nutzers, also gibt es
jedes Portal einmal. Der Gast-Modus nimmt einen Bus ohne Dienste, `docs/tty-dbus.conf`: Läuft eine andere Sitzung, würde
ein normaler Bus das Document-Portal doppelt starten und das der anderen Sitzung beenden, siehe `chain/done/08.md`. Gast
wird die Sitzung von selbst, wenn `$XDG_RUNTIME_DIR/doc` gemountet ist oder Mango läuft, oder mit `--gast`. Ohne
Dienste fehlen Schlüsselbund und Document-Portal, das ist nur für Tests.

Bleibt `$XDG_RUNTIME_DIR/doc` nach einem Absturz gemountet, ist die Sitzung ungewollt Gast. `session.log` sagt das, ein
Neustart behebt es.

## Mangos Autostart, bewertet

| Mango | wirbel |
|---|---|
| `dbus-update-activation-environment` | in `wirbel-session`, mit `--all` |
| `qs` shellwall, blobball, menu, salbei; launcher-host, widgets-host | `exec-once` wie gehabt |
| `wall-restore` (startet `awww-daemon`) | `exec-once`, wie gehabt |
| `wallsync.sh` | `exec-once`. Läuft nach dem Logout weiter, wie unter Mango |
| `polkit-gnome` | `exec-once` |
| `pipewire`, `wireplumber`, `pipewire-pulse` | in `wirbel-session`, mit Neustart für alle drei |
| `start-portals` | ersetzt. Es beendet Portale per `pkill -f` und träfe die einer anderen Sitzung |

## Grenzen
- Nach einem Logout aus Mango laufen dessen PipeWire und WirePlumber weiter, wirbel nimmt sie. Ton geht, WirePlumber hängt
  aber am toten Bus von Mango. Erst ein Neustart des Rechners gibt der Sitzung ihre eigenen. Ob das für etwas anderes als
  Bluetooth-Geräte oder Kamera-Rechte zählt, ist nicht geprüft.
- Das quickdots-Menü schreibt seinen Block nur in Mangos Config. Ein Wechsel der Bar dort kommt nicht an, und
  `set-blur.sh` und Co. haben unter wirbel keine Wirkung. Die Vorlage nennt `bars/salbei` fest.
- `mmsg` kann nur, was die Bars brauchen, `is_urgent` ist immer `false`, das Layout heißt immer `DW`.
