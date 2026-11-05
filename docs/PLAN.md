# wirbel — Plan

Minimaler Wayland-Compositor in Rust auf Basis von Smithay, mit Dwindle-Layout. Soll Mango im
Alltag ersetzen können. Einzige ungewöhnliche Funktion: Mausposition live abfragen und die Maus
per Befehl überall hinbewegen. Vorgaben des Nutzers (5.11.2026): Smithay, alltagstauglich,
Keybinds in Config-Datei, vom WM selbst verarbeitet. Wallpaper über vorhandenes `awww`.

## Bedienung
- `wirbel` startet den Compositor (Backend automatisch: im Wayland/X11-Fenster nested, sonst DRM).
- `wirbel pos` zeigt die Mausposition in EINER Zeile, die sich bei jeder Bewegung sofort
  aktualisiert (`\r`, kein Scrollen), bis Ctrl+C. Push über IPC-Abo, kein Polling. Globale
  Layout-Koordinaten mit Nachkommastellen (exakt), dazu Ausgang.
  `--once` einmal ausgeben; ohne TTY (Pipe) eine Zeile pro Änderung; `--json`.
- `wirbel move X Y` setzt die Maus absolut, `wirbel move --rel DX DY` relativ. Wird an den
  sichtbaren Bereich geklemmt, funktioniert über Ausgänge hinweg und schickt den Clients normale
  Enter/Motion-Events (Hover/Fokus stimmen danach).
- Config: `~/.config/wirbel/config`, einfaches Textformat, Neuladen per Keybind ohne Neustart.
  Standard-Belegung an `~/.config/mango/config.conf` angelehnt (Super+Return kitty, Super+W
  schließen, Super+1..9 Workspaces, Alt+Pfeile Fokus, Super+Shift+Pfeile tauschen).
- Fehlt die Config, gelten eingebaute Defaults, inklusive Beenden-Keybind (nie aussperren).

## Stil
Minimal heißt: keine Animationen, kein Blur, keine Fensterdeko außer Rahmen und Lücke, keine
Skriptsprache. Workspaces 1–9 je Ausgang. Keine Features ohne Bezug zu dieser Datei.

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
