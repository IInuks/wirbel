# Invisible windows: the approaches that were looked at

Goal: programs run and are driven like normal windows, but nothing of them shows on the visible
desktop, and the visible focus and workspaces never change. Each approach is answered on the same
eight questions. `DESIGN.md` holds what was chosen.

Two facts about Wayland shape everything below:

- A seat has one keyboard focus. Typing into a hidden window without taking the focus from the
  visible one needs another seat or another compositor.
- A client belongs to the connection it opened. A running program cannot be handed from one
  compositor to another.

## 1. Shadow instances: a hidden headless wirbel per window (chosen as the core)

1. How it works: the main wirbel starts the same binary again in a headless mode, one process
   per invisible window or group. The shadow renders on a GPU render node into a buffer nobody
   sees and has its own Wayland socket, seat, clipboard and Xwayland. The program is started
   with that socket as `WAYLAND_DISPLAY`.
2. Programs: every Wayland program, and X11 programs through the shadow's Xwayland.
3. Invisibility: complete. The visible compositor does not know the windows at all.
4. Wayland: native.
5. Limits: around 30 to 60 MB per shadow; already open windows cannot move in; programs that
   allow one instance only (D-Bus name, profile lock) hand the start to their visible copy
   unless they get a private bus or profile.
6. Rust: all of it is wirbel's own code (headless backend, input injection, screencopy).
7. Resources: one small process and one GL context each; frames are drawn only on change.
8. Integration: low risk. The daily compositor only gains a supervisor and requests.

## 2. Parked windows in the main compositor with private seats

1. How it works: hidden windows stay clients of the visible compositor, outside all workspaces,
   each with a seat of its own for independent input.
2. Programs: Wayland programs that bound the private seat at their start; X11 programs would
   still need an Xwayland of their own.
3. Invisibility: complete on screen; the programs still see the real outputs.
4. Wayland: native.
5. Limits: the Smithay version in use cannot show a seat to chosen clients only, so every client
   would see every seat, or Smithay has to be forked. Input, popup grabs and focus code of the
   daily compositor assume one seat.
6. Rust: yes, but against the grain of Smithay.
7. Resources: lowest of all.
8. Integration: high risk for the daily desktop. Rejected. Parking without private seats is
   kept for adopting windows that are already open (section 11).

## 3. Foreign headless compositors and virtual X servers (cage, sway, weston, Xvfb)

1. How it works: another program provides the hidden display, wirbel starts it and drives it
   with outside tools.
2. Programs: any (Xvfb: X11 only).
3. Invisibility: complete.
4. Wayland: yes for the compositors, no for Xvfb.
5. Limits: input and capture only through their protocols and helper tools; behaviour differs
   from wirbel; more to install.
6. Rust: only as process calls.
7. Resources: medium, a full foreign compositor each.
8. Integration: none to speak of. Rejected, the feature is meant to be part of wirbel.

## 4. Virtual input devices (uinput, virtual-keyboard and virtual-pointer protocols, libei)

1. How it works: synthetic keyboards and mice feed the compositor's normal seat.
2. Programs: whatever has the visible focus.
3. Invisibility: none, the events land where the user is working.
4. Wayland: yes.
5. Limits: cannot address a hidden window.
6. Rust: easy.
7. Resources: negligible.
8. Integration: not needed, wirbel injects input into a shadow directly. Rejected.

## 5. Accessibility interface (AT-SPI over D-Bus)

1. How it works: toolkits publish their widgets, texts and actions on the accessibility bus and
   send signals when they change.
2. Programs: GTK, Qt, Firefox, Chromium and Electron (with accessibility forced on),
   LibreOffice. Not games, not most GPU terminals.
3. Invisibility: no pixels involved, works on hidden windows.
4. Wayland: independent of it.
5. Limits: coverage depends on the program; trees of browsers are large; positions are
   unreliable under Wayland.
6. Rust: `zbus`.
7. Resources: low, but a program with accessibility on does a little more work.
8. Integration: medium, lives in the command line process, not in the compositor. Chosen for
   exact text, pressing controls and events such as "document loaded".

## 6. Clipboard trick inside the shadow

1. How it works: select all and copy are sent to the program, wirbel reads the shadow's own
   clipboard.
2. Programs: anything with text selection.
3. Invisibility: complete, the visible clipboard is another one.
4. Wayland: native.
5. Limits: changes the selection in the program; whole text only, no positions.
6. Rust: in tree.
7. Resources: none.
8. Integration: small. Chosen as the fallback that needs nothing installed.

## 7. Screen analysis and OCR

1. How it works: the picture of the shadow is compared (pixels, regions, reference images) or
   read by a text recogniser.
2. Programs: everything that draws, games included.
3. Invisibility: works on the hidden picture.
4. Wayland: independent.
5. Limits: OCR takes 0.2 to 1 s and can misread; needs `tesseract` installed.
6. Rust: comparison in tree, OCR as a helper program.
7. Resources: only when asked, and only after the picture changed.
8. Integration: small. Chosen for text with positions and for image waits.

## 8. Browser protocols (WebDriver BiDi, Chrome DevTools Protocol)

1. How it works: the browser opens a local WebSocket for remote control.
2. Programs: Firefox and the Chromium family.
3. Invisibility: independent of the window; the browser still runs with a real window in the
   shadow, which sites cannot tell from a normal one.
4. Wayland: independent.
5. Limits: browsers only; the port has to be asked for at start.
6. Rust: a WebSocket crate and a small JSON reader.
7. Resources: low.
8. Integration: medium, a late phase. Chosen for exact page text and load events.

## 9. Direct process control (pipes, PTY, signals, exit status, inotify)

1. How it works: the program is watched as a process: its output, how it ends, files it writes.
2. Programs: all, most useful for command line programs.
3. Invisibility: complete.
4. Wayland: independent.
5. Limits: says nothing about what a graphical program shows.
6. Rust: std and libc.
7. Resources: none.
8. Integration: small. Chosen for the events "exited" and "file created".

## 10. Sound: a PipeWire null sink per instance

1. How it works: each shadow gets a sink of its own, programs in it play into it; its monitor
   can be recorded, measured or looped to the speakers.
2. Programs: all that use PulseAudio or PipeWire.
3. Invisibility: silent unless routed to the speakers.
4. Wayland: independent.
5. Limits: needs the session's PipeWire; a virtual microphone for the program is possible but
   not planned.
6. Rust: helper programs `pactl` and `pw-record`; native bindings would tie the build to one
   library version.
7. Resources: low.
8. Integration: small, its own phase.

## 11. Adopting open windows: parking with limits

A window that already runs stays a client of the visible compositor. It can be taken off its
workspace and the taskbar and kept drawing, so screenshots, accessibility text and waiting work
fully. Keyboard and pointer exist only on the one visible seat, so input goes through
accessibility actions or by lending the focus for a moment, which the focused program notices.
That is why input into parked windows is off unless the configuration turns it on.

## What is possible and what is not

Works in practice: starting any program invisibly, several at once, typing, clicking, moving,
scrolling, screenshots, private clipboard, waiting on window, picture, file, process and sound
events, text by accessibility, clipboard, OCR and browser protocol, recording sound and video.

Only theoretical or unreliable: independent keyboard for an adopted window; moving a running
window into a shadow; a certain "page loaded" for programs with neither accessibility nor a
protocol (there the answer is a heuristic: title changed, picture stable, no busy cursor).
