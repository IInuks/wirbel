# wirbel — Plan

Minimaler Wayland-Compositor in Rust auf Basis von Smithay, mit Dwindle-Layout. Soll Mango im
Alltag ersetzen können. Einzige ungewöhnliche Funktion: Mausposition live abfragen und die Maus
per Befehl überall hinbewegen. Vorgaben des Nutzers (5.11.2026): Smithay, alltagstauglich,
Keybinds in Config-Datei, vom WM selbst verarbeitet. Wallpaper über vorhandenes `awww`.

## Bedienung
- `wirbel` startet den Compositor (Backend automatisch: im Wayland/X11-Fenster nested, sonst DRM,
  das aber nur mit einem VT auf stdin, sonst Exit 1). Echter Start und Testablauf: `docs/TTY-TEST.md`.
- `wirbel check [CONFIG]` prüft Session, Karten, Ausgänge, Rendern auf beiden GPUs und Eingabe, ohne
  etwas zu übernehmen. Darf immer laufen, auch im laufenden Mango. libseat wird dabei nicht gerufen,
  denn schon das Öffnen eines Sitzes aktiviert die Session und übernimmt sie.
- Notausgänge: `--timeout SEK` beendet sauber, und nach 5 s Hänger mit Gewalt (Exit 71).
  Strg+Alt+Shift+Rücktaste beendet sofort (Exit 70), auch gesperrt oder bei hängender Hauptschleife.
  Strg+Alt+Fn wechselt das VT, auch gesperrt, die Sperre bleibt. SIGTERM und SIGINT beenden sauber.
  Eine Panik loggt mit Backtrace, stellt den TTY wieder her (Exit 101). `--panic-after SEK` und
  `--hang-after SEK` lösen das zum Ausprobieren aus.
- Log: auf dem VT `~/.local/state/wirbel/wirbel.log` (das vorige wird `.old`), sonst stderr, mit
  `--log DATEI` immer dorthin. stderr und damit die Meldungen von Treibern und Clients folgen in die
  Datei.
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
- Gesperrt (`ext-session-lock`) gibt es nur noch die Lock-Surfaces: Der Bildschirm zeigt nichts sonst,
  Tasten und Zeiger erreichen nur sie, keine Binds, keine Fokuswechsel. `wirbel pos` und `wirbel move`
  bleiben erlaubt, der Zeiger bewegt sich, sieht aber nur die Lock-Surface. Stirbt der Locker, bleibt
  gesperrt, bis ein neuer Locker übernimmt; nested beendet nur das Schließen des Fensters die Sitzung.
- Config: `~/.config/wirbel/config` (oder `wirbel --config DATEI`), einfaches Textformat, Neuladen
  per Keybind ohne Neustart. Standard-Belegung an `~/.config/mango/config.conf` angelehnt
  (Super+Return kitty, Super+W schließen, Super+1..9 Workspaces, Alt+Pfeile Fokus,
  Super+Shift+Pfeile tauschen). Referenz aller Optionen und Aktionen: `src/default.conf`, die
  zugleich die eingebauten Defaults sind.
- Fehlt die Config, gelten eingebaute Defaults, inklusive Beenden-Keybind (nie aussperren). Das
  gilt auch für eine Config mit eigenen Binds, aber ohne `quit`: Super+M kommt dann dazu.
- Fehlerhafte Zeilen: beim Start übersprungen und geloggt. Beim Neuladen verwirft ein einziger
  Fehler die ganze Datei, die laufende Config bleibt.

- X11-Programme laufen über Xwayland, das mit dem Compositor startet. `DISPLAY` setzt wirbel nur für
  eigene Kinder, `wirbel -c` wartet auf nichts, der Socket steht vor dem Server. X11-Fenster liegen in den
  Workspaces wie alle anderen, Dialoge und Fenster fester Größe floaten, Floating-Fenster dürfen sich
  selbst bewegen. Stirbt Xwayland, verschwinden seine Fenster und es startet neu, höchstens 3-mal pro
  Minute. Option `xwayland`: `auto`, `off` oder ein Pfad. `wirbel check` zeigt, welches gefunden wurde.
- Der Zeiger kann gesperrt oder eingesperrt werden, solange sein Fenster die Tastatur hat. `wirbel move`
  gilt trotzdem: Der Zeiger bewegt sich, außerhalb der Surface endet die Sperre.
- Fenster bekommen per `xdg-activation` nur den Fokus, wenn der Token von einem frischen Eingabe-Event
  des Clients mit der Tastatur stammt, sein Workspace kommt dann nach vorn. Sonst passiert nichts: Eine
  Glocke in kitty stiehlt keinen Fokus. `_NET_ACTIVE_WINDOW` folgt derselben Regel, aus X11 heraus.

## Stil
Minimal heißt: keine Animationen, kein Blur, keine Fensterdeko außer Rahmen und Lücke, keine
Skriptsprache. Workspaces 1–9 je Ausgang. Keine Features ohne Bezug zu dieser Datei. Fenster malen
keine eigene Deko: Egal was sie verlangen, `xdg-decoration` antwortet immer serverseitig.

## Aufbau
- `src/safety.rs`: das Sicherheitsnetz, unabhängig vom Backend und von der Hauptschleife. Eigene
  Threads: `Watchdog` (Deadline, Signale, meldet eine stehende Schleife) und `watch_keyboards` (liest
  die Notaus-Taste selbst aus `/dev/input`, ohne Grab, nur mit Gruppe `input`; Tasten anderer VTs werden
  verworfen). `on_key` prüft die Taste zusätzlich, nested läuft nur das, außer mit
  `WIRBEL_EMERGENCY_DEVICES=PFAD:PFAD` (für Tests). Alles endet in `hard_exit`: `_exit` ohne Destruktoren,
  der Kernel schließt die Geräte und elogind gibt den VT zurück, `restore_terminal` tut das vorab per
  ioctl selbst. Einen in einem Treiber-ioctl hängenden Prozess kann auch das nicht beenden.
- `src/check.rs`: `wirbel check`. Geht den Weg von libseats logind-Backend (libelogind oder libsystemd
  per dlopen, je nach Stratum) bis vor `Activate`. Karten nur lesend, ohne erzwungenen Probe, ein
  ungewollt zugefallener DRM-Master geht sofort zurück. Der Render-Test läuft auf Render-Nodes mit
  `MultiRenderer`, also genauso wie ein echtes Bild, bis auf den letzten Schritt zum Bildschirm.
- `src/focus.rs`: `KeyboardFocus`, Wayland-Surface oder X11-Fenster. X11-Fenster müssen den Fokus auch auf
  der X-Seite erfahren und haben anfangs noch keine Surface. Der Zeiger braucht das nicht, sein Ziel
  bleibt die `WlSurface`.
- `src/xwayland.rs`: findet das Binary: `PATH`, `/bedrock/cross/bin`, dann `strat STRATUM /usr/bin/Xwayland`,
  jeder Kandidat muss `-version` schaffen. Smithay startet nur `Xwayland` aus dem `PATH`, ein anderes läuft
  über ein Skript in einem eigenen Ordner, das bis zum Ready bleiben muss und den Namen nicht nochmal
  per `PATH` auflösen darf.
- `src/monitor.rs`: Mode-Wahl, Position, EDID-Name, frei von DRM-Typen. `src/cursor.rs`: Cursor-Thema,
  mit Adwaita und einem eingebauten Pfeil als Rückfall. Animierte Cursor zeigen ihr erstes Bild.
- `src/layout/`: Dwindle-Baum und Geometrie, frei von Compositor-Typen. Die Split-Achse wird nicht
  gespeichert, sondern folgt aus der Form der Fläche.
- `src/workspace.rs`: `Monitor` (ein Ausgang mit 9 Workspaces) und `Workspace` (Dwindle-Baum,
  Floating-Stapel, Fullscreen/Maximize, gemerkter Fokus). `Workspace::frames` berechnet, was wo
  sichtbar ist: Fullscreen bekommt den ganzen Ausgang, alles andere `work_area`, den Teil ohne
  Exclusive Zones. Fenster haben keinen eigenen Status außerhalb dieser Struktur.
- `src/state.rs`: `Wirbel`, zentraler Zustand. `monitors` hält alle Fenster, `space` nur die
  sichtbaren. `retile()` leitet den ganzen Bildschirm aus `monitors` ab: blendet ein und aus, setzt
  die Stapelreihenfolge, schickt Configures. Nach jeder Änderung an Workspaces aufrufen.
- `src/config.rs`, `src/default.conf`: Config-Parser und Defaults.
- `src/actions.rs`: was Binds tun (`run_action`).
- `src/ipc.rs`: Steuer-Socket `$XDG_RUNTIME_DIR/wirbel-<WAYLAND_DISPLAY>.sock`, Zeilenprotokoll
  (oben in der Datei beschrieben), läuft nicht blockierend in der Event-Loop. `src/cli.rs`: die
  Befehle `wirbel pos` und `wirbel move`.
- `Wirbel::under` (`src/state.rs`) ist die eine Antwort auf "was liegt an diesem Punkt", gesperrt nur
  Lock-Surfaces. `surface_under` und `window_at` lesen daraus, Hover, Klick, sloppy focus und
  `wirbel move` stimmen also von selbst.
- `Wirbel::keyboard_target` ist die eine Antwort auf "wer bekommt die Tastatur": Lock-Surface, dann
  Layer mit `exclusive` (Top/Overlay), dann per Klick gewählter Layer (`on_demand`), dann das
  fokussierte Fenster. Wer an der Antwort etwas ändert, ruft `update_keyboard_focus`. Sloppy focus
  nimmt einem Layer die Tastatur nicht, ein Klick woanders schon.
- `src/grabs.rs`: Fenster ziehen. Der Pointer-Grab hält nur Clients fern, die Logik läuft danach in
  `on_pointer_motion`/`on_pointer_button`, denn Code im Grab darf den Pointer nicht anfassen und
  `retile` tut das.
- Popup-Grabs: `popup_grab` hält Smithays Grab. `reap_popup_grab` läuft nach jeder Loop-Iteration und
  beendet einen Grab, dessen Popups weg sind, bevor Smithays Grab die nächste Bewegung schluckt
  (`wirbel move` ginge sonst verloren). Wird ein Fenster oder Layer unsichtbar, zieht
  `dismiss_popups` seine Popups zurück, ein Fokuswechsel beendet den Grab. Popups von Fenstern
  bleiben in der `work_area`, Popups von Layern dürfen auf den ganzen Ausgang.
- `src/handlers/xwayland.rs`: X11-Fenstermanager und Start von Xwayland. Die Startpipe liest wirbel selbst
  (`take_socket`): Smithays Event-Quelle meldet einen Tod beim Start nicht und dreht auf der geschlossenen
  Pipe. `retile` sagt X11-Fenstern Größe und Zustand (`configure_x11`), `overrides` hält Menüs und
  Tooltips, die sich selbst platzieren. `settle_x11_windows` läuft nach jeder Loop-Iteration, weil der erste
  Buffer vor der Zuordnung zum X-Fenster ankommen kann. X11 kennt kein Ack: Ein Commit zählt in
  `floating_commit` nur, wenn seine Größe zum letzten Configure passt.
- `src/handlers/`: Wayland-Protokolle. `layer_shell.rs`: ein Layer liegt bis zum ersten Commit in
  `unmapped_layers`, ein Commit ohne Buffer schickt ihn dorthin zurück, und der Client beginnt von
  vorn, wie das Protokoll es verlangt. `session_lock.rs`: `Phase` Unlocked, Locking (wartet auf ein
  präsentiertes leeres Bild je Ausgang), Locked. Der `locked`-Event geht erst danach raus, gesperrt
  ist aber schon ab der Anfrage. `selection.rs`: Clipboard, primary selection, Drag and Drop.
  `mod.rs`: der Rest, darunter `offer_dmabufs`. Idle: `ext-idle-notify` gehört mit dazu, ohne ihn
  hätte `idle-inhibit` keine Wirkung. Ein Inhibitor zählt nur, solange seine Surface sichtbar ist.
  Lebenslauf eines Toplevels: erster Commit platziert es,
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
- `src/render.rs`: Szene unabhängig vom Backend (`scene_elements`), von vorn nach hinten: Cursor und
  DnD-Icon, Overlay, Top, Popups aller Layer darunter, Fenster mit Rahmen, Bottom, Background.
  Fullscreen liegt über Top, nicht über Overlay. `layer_tiers` liefert diese Ordnung, auch für
  `under`. Gesperrt zeichnet die Szene nur Cursor und Lock-Surface. Backends setzen `state.redraw`
  und zeichnen nur auf Anforderung.
- `src/backend/winit.rs`: nested, ein Ausgang. `add_output`, `remove_output` und `output_resized` sind
  die Schnittstelle für Backends; ein Ausgang liegt dort, wo `current_location` sagt. `wirbel -c BEFEHL`
  startet Clients beim Start. Ein Backend, das dmabufs kann, setzt `import_dmabuf` und ruft
  `offer_dmabufs`. Hooks in `Wirbel`, die es setzen kann: `redraw`, `change_vt` (macht Strg+Alt+Fn erst
  aktiv), `early_import`, `led_update`, `cursors`.
- `src/backend/drm.rs`: libseat, udev, GBM, libinput, alle GPUs des Sitzes. Eine GPU rendert (`render_gpu`,
  sonst die Boot-GPU, hier die AMD-iGPU, wie wlroots), `MultiRenderer` kopiert auf die anderen. Der
  gemeinsame Zustand liegt in `Rc<RefCell<Drm>>`, nie über einen Aufruf hinweg geborgt, der wieder
  einen Hook auslöst. NVIDIA: keine Overlay-Planes. Kein Ausgang beim Start: Exit 1 statt Schwarzbild.
  Eine `monitorrule` wird beim Erscheinen des Ausgangs gelesen; `reload` ändert laufende Modes nicht.
  Wird ein Mode abgelehnt, versucht das Backend den Standard-Mode. Gesperrt holt der VT-Wechsel die
  Ausgänge mit abgeschalteten Connectors zurück, damit kein altes Bild aufblitzt.
- Nach einem Bild: `update_scanout_outputs` merkt, wo eine Surface zuletzt zu sehen war, darauf bauen
  `presentation_feedback` und `send_dmabuf_feedback` auf (`src/render.rs`). Pro Ausgang gibt es Feedback
  fürs Zeichnen und eins mit Scanout-Tranche, `render_frame` setzt passende Buffer selbst aufs Primary Plane.
  Das Log meldet `direct scanout` bei jedem Wechsel. Direct Scanout setzt voraus, dass der Client auf der GPU
  mit dem Monitor rendert. Mit AMD-Clients und Monitor an der NVIDIA kommt es nicht vor. Explicit Sync
  (`offer_syncobj`) bietet das DRM-Backend auf der Render-GPU an, nested auf deren Render-Node.
- Tearing fehlt: Smithay kann keine Async-Flips, `DrmSurface::page_flip` setzt kein `PAGE_FLIP_ASYNC`, und
  `wp_tearing_control_v1` hat es auch nicht. Das Protokoll allein brächte nichts, also ist es nicht angeboten.
- `src/backend/pacing.rs`: pro Ausgang höchstens ein Bild unterwegs, das nächste nach dem Vblank plus
  60 % Bilddauer für Clients (sofort bei einer Kopie auf eine andere GPU). Ändert sich nichts, ersetzt
  ein Timer den Vblank, damit Frame-Callbacks im Takt bleiben. `send_frame_callbacks` zählt für die
  Sperre; das DRM-Backend ruft stattdessen `send_frames` und nur für ein unter der Sperre gezeichnetes
  Bild `lock_frame_presented`, ein vorher eingereihtes Bild zählt nicht.
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
- Echte Layer-Clients sauber trennen: `awww-daemon` nur mit `WAYLAND_DISPLAY=wayland-N` und eigenem
  `XDG_CACHE_HOME` starten, dann bekommt er einen eigenen Socket. Die Quickshell-Bar aus einer Kopie
  des Ordners starten, mit eigenem `HOME` und `XDG_RUNTIME_DIR` (kurzer Pfad, sonst passt der
  IPC-Socket nicht), `WAYLAND_DISPLAY` als absolutem Pfad, unter `dbus-run-session` (sonst übernimmt
  sie die Benachrichtigungen) und mit Attrappen für `mmsg` und `mpvpaper` vorn im `PATH`. Beenden
  über die eigenen PIDs samt Kindern.
- Für Sperre und Popups reicht eine kleine QML-Datei mit `WlSessionLock` bzw. `PopupWindow`, das
  ergibt echte Clients. Klicks lassen sich nested nicht auslösen, ohne die Maus des Nutzers zu bewegen.
- Das `[EGL] BAD_SURFACE`-Log nested stammt aus `buffer_age`: Nach dem Import eines neuen Buffers ist
  die Fensterfläche nicht aktuell, der Rahmen wird dann ganz neu gezeichnet. Harmlos.
- Echte Shell in nested kitty: `kitty -o allow_remote_control=yes --listen-on unix:PFAD`, dann
  `kitty @ --to unix:PFAD send-text`, `send-key ctrl+c`, `get-text`. Das geht in den PTY wie eine Taste.
- Die nested Maus liefert nur absolute Positionen. Relative Bewegung und Sperren prüft `on_pointer_delta`
  in `tests/headless.rs`, die echte Maus `docs/pointer-test.html` im TTY-Nachtest.
- Prozesse immer per gemerkter PID beenden: `pgrep -f MUSTER` trifft auch die eigene Shell.
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
Gestartet wird das DRM-Backend aber mit `strat void`: Nur Void hat die NVIDIA-Userspace-Treiber (EGL,
`nvidia-drm_gbm.so`), in Arch fällt `renderD129` auf llvmpipe zurück. Die Arch-Binary läuft dort, weil sie
höchstens glibc 2.39 verlangt und Void 2.41 hat; libdisplay-info ist darum nicht verlinkt, die
Strata haben verschiedene Versionen. libseat und udev funktionieren in beiden. Clients kommen weiter
aus Arch über `/bedrock/cross`, dort nur Mesa: Sie rendern auf der AMD-iGPU, wie unter Mango. Das gilt
auch für Steam und Spiele, Arch hat keinen NVIDIA-Vulkan-Treiber (nur `radeon_icd`). Xwayland gibt es in Arch
und Void: Mit `strat void` gestartet nimmt wirbel das aus Void, das auch die NVIDIA-Treiber sieht.
GPUs: NVIDIA RTX 4070 SUPER (Treiber 595, open) und AMD iGPU; Monitor an card1 (DP-4, wechselt
gelegentlich zu DP-3), 165 Hz (der Monitor kann 170, Mango nutzt 170).
