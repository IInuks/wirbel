# wirbel — Plan

Minimaler Wayland-Compositor in Rust auf Basis von Smithay, mit Dwindle-Layout. Soll Mango im
Alltag ersetzen können. Einzige ungewöhnliche Funktion: Mausposition live abfragen und die Maus
per Befehl überall hinbewegen. Vorgaben des Nutzers (5.11.2026): Smithay, alltagstauglich,
Keybinds in Config-Datei, vom WM selbst verarbeitet. Wallpaper über vorhandenes `awww`.

## Bedienung
- `wirbel` startet den Compositor (Backend automatisch: im Wayland/X11-Fenster nested, sonst DRM).
- `wirbel pos` zeigt die Mausposition in EINER Zeile, die sich bei jeder Bewegung sofort
  aktualisiert (`\r`, kein Scrollen), bis Ctrl+C. Push über IPC-Abo, kein Polling. Globale
  Layout-Koordinaten mit Nachkommastellen (exakt), dazu Ausgang (`-`, wenn es keinen gibt).
  `--once` einmal ausgeben; ohne TTY (Pipe) eine Zeile pro Änderung; `--json` eine Zeile JSON pro
  Änderung. Wer nicht mitliest, verpasst Zwischenstände, bekommt aber zuletzt die aktuelle Position.
- `wirbel move X Y` setzt die Maus absolut, `wirbel move --rel DX DY` relativ. Wird an den
  sichtbaren Bereich geklemmt (nächster Punkt auf einem Ausgang, jenseits einer Kante deren letztes
  ganzes Pixel), funktioniert über Ausgänge hinweg und schickt den Clients normale
  Enter/Motion-Events (Hover/Fokus stimmen danach).
- `wirbel move` wirkt wie Bewegen der Maus, auch während eines Grabs: Ein gezogenes Fenster folgt
  und landet beim Loslassen dort, ein Teiler oder eine Fenstergröße ändert sich mit. Bei gehaltener
  Taste über einem Client bleibt der Zeiger bei diesem Client, bis die Taste losgelassen wird. Der
  Befehl wird nie abgelehnt, solange ein Ausgang existiert; nur `warp_cursor` wartet einen Grab ab.
- `wirbel pos` und `wirbel move` sprechen den Compositor von `WAYLAND_DISPLAY` an, aus Programmen
  in wirbel also von selbst den richtigen. Ohne Compositor: Exit 1 und Meldung auf stderr.
- Config: `~/.config/wirbel/config` (oder `wirbel --config DATEI`), einfaches Textformat, Neuladen
  per Keybind ohne Neustart. Standard-Belegung an `~/.config/mango/config.conf` angelehnt
  (Super+Return kitty, Super+W schließen, Super+1..9 Workspaces, Alt+Pfeile Fokus,
  Super+Shift+Pfeile tauschen). Referenz aller Optionen und Aktionen: `src/default.conf`, die
  zugleich die eingebauten Defaults sind.
- Fehlt die Config, gelten eingebaute Defaults, inklusive Beenden-Keybind (nie aussperren). Das
  gilt auch für eine Config mit eigenen Binds, aber ohne `quit`: Super+M kommt dann dazu.
- Fehlerhafte Zeilen: beim Start übersprungen und geloggt. Beim Neuladen verwirft ein einziger
  Fehler die ganze Datei, die laufende Config bleibt.

## Stil
Minimal heißt: keine Animationen, kein Blur, keine Fensterdeko außer Rahmen und Lücke, keine
Skriptsprache. Workspaces 1–9 je Ausgang. Keine Features ohne Bezug zu dieser Datei.

## Aufbau
- `src/layout/`: Dwindle-Baum und Geometrie, frei von Compositor-Typen. Die Split-Achse wird nicht
  gespeichert, sondern folgt aus der Form der Fläche.
- `src/workspace.rs`: `Monitor` (ein Ausgang mit 9 Workspaces) und `Workspace` (Dwindle-Baum,
  Floating-Stapel, Fullscreen/Maximize, gemerkter Fokus). `Workspace::frames` berechnet, was wo
  sichtbar ist. Fenster haben keinen eigenen Status außerhalb dieser Struktur.
- `src/state.rs`: `Wirbel`, zentraler Zustand. `monitors` hält alle Fenster, `space` nur die
  sichtbaren. `retile()` leitet den ganzen Bildschirm aus `monitors` ab: blendet ein und aus, setzt
  die Stapelreihenfolge, schickt Configures. Nach jeder Änderung an Workspaces aufrufen.
- `src/config.rs`, `src/default.conf`: Config-Parser und Defaults.
- `src/actions.rs`: was Binds tun (`run_action`).
- `src/ipc.rs`: Steuer-Socket `$XDG_RUNTIME_DIR/wirbel-<WAYLAND_DISPLAY>.sock`, Zeilenprotokoll
  (oben in der Datei beschrieben), läuft nicht blockierend in der Event-Loop. `src/cli.rs`: die
  Befehle `wirbel pos` und `wirbel move`.
- `src/grabs.rs`: Fenster ziehen. Der Pointer-Grab hält nur Clients fern, die Logik läuft danach in
  `on_pointer_motion`/`on_pointer_button`, denn Code im Grab darf den Pointer nicht anfassen und
  `retile` tut das.
- `src/handlers/`: Wayland-Protokolle. Lebenslauf eines Toplevels: erster Commit platziert es,
  erster Buffer fokussiert, Commit ohne Buffer entfernt es wieder (`toplevel_commit`). Dialoge
  (Parent gesetzt) und Fenster fester Größe floaten von selbst.
- `Wirbel::move_pointer` (`src/state.rs`) ist der einzige Aufrufer von `pointer.motion`: klemmt an
  die Ausgänge und meldet die Position an `wirbel pos`. Wer den Zeiger bewegt, geht hier durch, auch
  künftige Protokolle. Die Abo-Zeile entsteht beim Vergleich mit der zuletzt gemeldeten Position,
  daher sind Aufrufe ohne Änderung still und ein neuer Ausgang unter dem Zeiger fällt trotzdem auf.
  `on_pointer_motion` ist die ganze Mausbewegung samt Drag und sloppy focus, `wirbel move` nimmt
  diesen Weg; `warp_pointer` ist die Bewegung des Compositors selbst und tut im Grab nichts.
- `src/input.rs`: Backends rufen `on_key`, `on_pointer_motion`, `on_pointer_button`,
  `on_pointer_axis`; dort speisen auch Tests Eingaben ein. Keybinds greifen im Filter von
  `keyboard.input`, gebundene Tasten erreichen Clients weder beim Drücken noch beim Loslassen.
- `src/render.rs`: Szene unabhängig vom Backend (`scene_elements`), von vorn nach hinten: Cursor,
  dann je Fenster es selbst und sein Rahmen. Backends setzen `state.redraw` und zeichnen nur auf
  Anforderung.
- `src/backend/`: bisher nur `winit` (nested), mit einem Ausgang. `add_output`, `remove_output` und
  `output_resized` sind die Schnittstelle für weitere Backends; ein Ausgang liegt dort, wo
  `current_location` sagt. `wirbel -c BEFEHL` startet Clients beim Start.
- `tests/headless.rs`: Compositor ohne Backend, echter Wayland-Client über den Socket, Eingaben
  injiziert, Pixel per Pixman geprüft, `wirbel pos`/`move` als echtes Binary. Hier gehören neue
  Verhaltenstests hin. Socket-Nummern werden im parallelen Lauf wiederverwendet: Ein Test darf nicht
  annehmen, dass eine Nummer nach dem Beenden frei bleibt.
- Smithay ist auf einen Git-Commit gepinnt (513 Commits nach 0.7.0), winit also 0.31-beta.

## Hinweise für Tests im laufenden Mango
- Fenster laufen als `strat -r arch ./target/debug/wirbel`; Host-Fenster finden über
  `mmsg get all-clients` (Titel `wirbel`), Screenshot per `grim -g` nur auf dieses Rechteck.
- Mango fängt Super-Kombinationen ab, Super+M beendet die Sitzung des Nutzers: im Nested-Test nie
  senden. Fürs saubere Beenden `mmsg dispatch killclient client,ID`; SIGTERM lässt den Socket liegen.
- `wtype` lädt eine eigene Keymap, die wirbel nicht auswertet: Tasten kommen an, aber als Evdev-Code
  1, 2, 3, ... (ESC, 1, 2, ...), N verschiedene Zeichen drücken die Codes 1 bis N. Modifier sendet es
  nicht als Tasten. Binds testet man so: `bind = NONE, 1, reload` und `bind = NONE, 2, AKTION`, die
  Aktion in der Datei ändern, dann `wtype abc`. Vorher per `mmsg get focusing-client` prüfen, dass
  das Nested-Fenster den Host-Fokus hat.
- Mit `WAYLAND_DISPLAY=wayland-N ./target/debug/wirbel pos` bzw. `move` lässt sich die Nested-Instanz
  von außen steuern, ohne die Maus des Nutzers zu bewegen. Der Socket steht im Log.

## Etappen
1. Grundgerüst: Crate, Smithay, verschachteltes Winit-Backend, xdg-shell, Rendering, Tastatur/Maus,
   Dwindle-Baum mit Unit-Tests, kitty läuft nested.
2. Config + Keybinds, Workspaces, Fokus/Tausch, Split-Verhältnis, Floating, Fullscreen, Maus-Resize.
3. IPC-Socket, `wirbel pos`, `wirbel move`, Integrationstests.
4. Protokolle Teil 1: layer-shell (awww, Quickshell, exclusive zones), xdg-popup komplett,
   xdg-decoration, data-device, primary-selection, cursor-shape, viewporter, fractional-scale,
   xdg-output, dmabuf, session-lock, idle-inhibit.
5. DRM-Backend: libseat/elogind, udev, GBM, libinput, VRR-frei, Vblank-Pacing, Hotplug, VT-Wechsel,
   Multi-GPU (Monitor an RTX 4070S, iGPU AMD). Echter Test nur mit Nutzer, siehe unten.
6. XWayland (Smithay xwm): Steam, Discord, Spiele; pointer-constraints, relative-pointer,
   tearing/direct scanout für Fullscreen-Spiele.
7. Screenshots/Screenshare: wlr-screencopy, foreign-toplevel, xdg-desktop-portal, flameshot/grim.
8. Session: `wirbel-session`, Autostart, Eintrag für quickdots-Greeter, Doku, Migration von Mango.
9. Härtung: Review, Absturzpfade, Leaks, Client-Fehlverhalten, Performance.

Sessions dürfen Etappen teilen oder verschmelzen; der Manager führt die Reihenfolge in
`chain/QUEUE.md`.

## System
Bedrock Linux: Init Void (runit, elogind, KEIN systemd), Strata Arch/Debian. Bauen im Arch-Stratum,
das die Wayland-Dev-Libs hat: `strat -r arch env PATH=$HOME/.cargo/bin:$PATH cargo build`.
GPUs: NVIDIA RTX 4070 SUPER (Treiber 595, open) und AMD iGPU; Monitor an card1 (DP-4, wechselt
gelegentlich zu DP-3), 165 Hz. Sitzungsnahe Tools können cross-stratum zicken, vor allem
elogind/libseat: prüfen, ob Arch-libseat aus der Session funktioniert oder der Build/Start im
Void-Stratum liegen muss.
