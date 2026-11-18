# VWM progress

This file says where the work on invisible windows stands. Whoever continues reads it first,
then `DESIGN.md`, and carries on at "Next action".

## Rules

- Work on branch `vwm`. Small commits, one per finished step; the tree builds and the tests pass
  at every commit. Tick the step here in the same commit and rewrite "Next action".
- Nothing is pushed. No personal data in commits: no home paths, names, hosts, addresses, tokens.
- Tests are headless only and never talk to a running desktop: set `WAYLAND_DISPLAY` to the test
  compositor before any `wirbel` call.
- Checks per step: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.

## Next action

What is left, in the order of use:

1. Install and try on a real desktop (nothing was run there yet, only in a headless wirbel):
   the checklist is at the end of this file.
2. If `tesseract` gets installed: try `wirbel vwm text NAME --via ocr` on a real window and tune
   `vwm::text::recognise` (see the notes).
3. Step 11.1, the viewer `vwm show|hide`: a normal window on the desktop that shows an
   invisible one live and passes keys and pointer on. Plan: a small Wayland client in the
   command (crates `wayland-client` and `wayland-protocols`, already optional dependencies of
   the test harness) that draws the pictures of `shadow shot` into shm buffers and sends what
   it gets from its seat as `inject at`, `inject button` and a new `inject code EVDEV down|up`.
4. Step 7.4, input into parked windows, if it is wanted at all.

## Phases

### 0 Docs
- [x] 0.1 RESEARCH.md, DESIGN.md, PROGRESS.md

### 1 Shadow core
- [x] 1.1 `--shadow NAME [WxH]` start mode: headless, injection on, no config, no binds, no
      borders or gaps, named control socket
- [x] 1.2 single layout in a shadow: first window fills the output, later ones float centred
- [x] 1.3 manager in the main compositor: registry, start, supervision, cleanup at exit
- [x] 1.4 requests `vwm open|list|info|close|resize|where` on the control socket
- [x] 1.5 command line `wirbel vwm open|list|info|close|resize` and the `invisible` aliases
- [x] 1.6 private D-Bus session bus per shadow, `--share-bus`, `--env`
- [x] 1.7 program recipes (Firefox, Chromium family) with profiles in the state folder
- [x] 1.8 integration tests `tests/vwm.rs`

### 2 Control
- [x] 2.1 absolute pointer requests: move, click, drag, scroll
- [x] 2.2 `capture` request writing a PNG
- [x] 2.3 clipboard get and set
- [x] 2.4 command line `vwm type|key|click|move|drag|scroll|shot|clip`
- [x] 2.5 tests

### 3 Events and waiting
- [x] 3.1 shadow events: damage, client exit, cursor, clipboard
- [x] 3.2 wait engine with deadlines; conditions ready, window, title, closed, exited, stable,
      changed, pixel, image, file
- [x] 3.3 command line `vwm wait|events`
- [x] 3.4 tests

### 4 Macros
- [x] 4.1 lexer and parser with line numbers
- [x] 4.2 interpreter: values, variables, expressions, control flow, functions, try
- [x] 4.3 vwm steps, `on` handlers, timeouts
- [x] 4.4 `wirbel macro run|check|list`, bind action `macro`
- [x] 4.5 MACROS.md, examples, tests

### 5 Text I
- [x] 5.1 clipboard trick
- [x] 5.2 OCR through `tesseract`, positions, `wait text`

### 6 Text II
- [x] 6.1 AT-SPI: tree, text, press, set
- [x] 6.2 AT-SPI signals, `loaded`

### 7 Adoption
- [x] 7.1 parked list, hidden from taskbar, frame callbacks
- [x] 7.2 offscreen capture
- [x] 7.3 `adopt`, `release`, `windows`
- [x] 7.4 input into parked windows: focus borrowing behind `vwm_adopt_input = borrow`
      (`vwm input NAME REQUEST` on the desktop; keys past binds, pointer events on the window's
      surface; kept 300 ms after the last request, given back at once on real input; refused
      while keys or buttons are held, the pointer is grabbed or the session is locked). The CLI
      sends `type`, `key`, `click`, `scroll` ... for a parked name there; `text --via ocr` and
      `click-text` work on parked windows too. Pointer events go to the main surface only, not
      to popups of the window.

### 8 Audio
- [x] 8.1 null sink per instance, `record`, `listen`, `level`, sound and silence events

### 9 Video
- [x] 9.1 `record` through `ffmpeg`, frame export

### 10 Browser protocol
- [x] 10.1 `vwm web`, `loaded` and `text` through it

### 11 Viewer and finish
- [x] 11.1 `vwm show|hide`: `src/vwm/viewer.rs`, a Wayland client of the desktop (feature
      `vwm-viewer`, default on). Pictures as bare RGBA (`shadow shot PATH.rgba`) whenever the
      damage count moves, drawn bilinear to fit; input as `inject at|button|wheel` and the new
      `inject code EVDEV down|up`. `show` starts `wirbel vwm viewer NAME` in its own session
      and keeps its pid in `viewer.pid`; it ends with its window, `hide`, or the shadow.
- [x] 11.2 limits (32 shadows at once), memory column in `vwm list`, README section, GUIDE.md

## Notes and known problems

- Tests of shadows (`tests/vwm.rs`) start the real binary headless and need a GPU render node.
  The harness compositor plays the desktop; `Manager::binary` and `Manager::state_root` point it
  at the test binary and a scratch folder.
- `vwm open` answers once the shadow takes requests, not once the program has a window. Until
  `vwm wait NAME window` exists (phase 3), scripts sleep or poll `vwm info`.
- A command that cannot be started still gives a shadow, which ends after its 3 s of grace; the
  reason is in its `shadow.log`.
- The browser recipes (own profile for Firefox and the Chromium family) are covered by unit
  tests only; try a real browser in the smoke script of phase 2.
- `inject type` reaches the four levels of a key (plain, Shift, AltGr, both). Text the layout
  has no key for goes through `vwm paste` (clipboard and CTRL+v).
- Pictures and clipboard text travel as files named in the request, not over the socket: a
  request line is at most 4096 bytes. Their answers come late, through `Wirbel::reply_sender`
  or after the next frame.
- `shadow shot` works in the headless backend only (`vwm::shot::take` is called from its
  render). Parked windows (phase 7) need the same from the other backends.
- The clipboard type of the compositor is `vwm::clip::Held`: `None` stands for the X server,
  `Some` is text set from outside.
- Waiting polls instead of listening: `shadow status` answers damage count, quiet time, window
  count, program state, cursor, clipboard count and title in one line, and `vwm::wait` asks it
  every 40 ms. Pictures are taken only when the damage count moved. That replaced the pushed
  events and inotify of the design: same behaviour, far less code. `vwm events` prints the
  differences between two looks.
- A moving pointer and a blinking text cursor are damage too, so `stable` needs a time shorter
  than the blink of a terminal, and `changed` without a region fires on them.
- Still to come as conditions: `text GLOB` (phase 5), `loaded` (phase 6 and 10), `sound` and
  `silence` (phase 8). `Condition::parse` and `check` in `src/vwm/wait.rs` are the place.
- Macros reuse the commands: `vwm::cli::capture` runs `wirbel vwm ARGS` and returns what it
  would print, so a new command is a macro step once its word is in `vwm::cli::COMMANDS`.
  `wait` is the exception (handlers run during it). Tests of the interpreter use a fake
  `macros::interp::Host`.
- No bind action `macro`: `bind = MODS, KEY, spawn, wirbel macro run NAME` does it.
- `wirbel vwm close` returns once the window is gone, so its name is free again.
- `examples/macros/page-title.wm` is checked for syntax only; run it with a real browser in
  the smoke test.
- Text recognition was written without `tesseract` on the machine: only the reading of its TSV
  output is tested. Still to try with the real program: whether small interface fonts need the
  picture enlarged first, and whether `--psm 11` (sparse text) reads windows better than the
  default. `WIRBEL_OCR_LANG` or `--lang` picks languages.
- `vwm text --via clipboard` presses CTRL+a and CTRL+c; terminals do not select all that way.
- A headless wirbel that renders on a card of the `nvidia` driver crashed (SIGSEGV) as soon
  as Firefox connected, but only where the NVIDIA user space libraries are missing: there Mesa
  takes the card and jumps into code that is gone (seen in a core dump: `libEGL_mesa` and
  `libgallium` on the nvidia node). With the NVIDIA libraries present the same shadow runs.
  Shadows still prefer a card that is not nvidia when there are several
  (`manager::quiet_card`): it is enough for windows nobody looks at, leaves the other card to
  the screen, and avoids that trap. `vwm_gpu` in the config overrides.
- Tried with real programs (headless desktop, see below): kitty (type, paste, drag, shot) and
  Firefox with its own profile; `examples/macros/page-title.wm` returned the title of
  example.com.
- Accessibility (`src/vwm/a11y.rs`, feature `vwm-a11y`, crate `zbus`) asks instead of
  listening: `wait loaded` reads the tree again while the picture rests, which is a few
  hundred D-Bus calls for a browser. Signals would be cheaper; not needed so far.
- Tried with Firefox: `ui tree`, `text --via a11y`, `wait loaded`, `ui where` and `ui click`
  are right. `ui press` on a link of a page answers ok and does nothing there, also when the
  link is in view: links take `ui click` (scrolls the control into view, then clicks its
  middle with the pointer) or `web click`.
- With `open --share-bus` the accessibility tree is the one of the whole desktop, not of the
  window alone. Filtering by the processes of the shadow is still to do.
- Parked windows (`src/vwm/park.rs`) are held windows of the core (`Wirbel::held_windows`)
  with a name. An extension that sets a window hook and shows all held windows would show the
  parked ones too; it has to skip `Wirbel::is_parked`. The 3D town does that in its
  `src/windows.rs` (it chains `held_windows`); it still compiles against this branch.
- `status`, `wait`, `text` and `ui` do not work on parked windows yet: they go to the control
  socket of a shadow. Pictures of parked windows are taken by every backend
  (`vwm::shot::take_parked`), tried on the headless one with a real terminal.
- Sound (`src/vwm/audio.rs`) is done by the command, not the compositor: `open` makes a null
  sink `wirbel-vwm-NAME` with `pactl` and starts the program with `PULSE_SINK`; the module
  number is in the state folder of the window, `close` and the next `open` unload what is left
  over. A window that ends by itself keeps its sink until then. Windows opened by a macro get
  one too (macros run the same `open`); windows opened through the socket alone do not.
- `audio listen on` was not tried by ear. `sound`, `silence`, `level` and `record` were tried
  with a tone played in an invisible terminal.
- `vwm record` feeds PNG pictures to `ffmpeg -f image2pipe`, a new one only when the window
  changed, each at its time. Tried: mp4 with sound and gif of an invisible terminal. No frame
  export of its own: `shot` in a loop does that.
- The browser protocols (`src/vwm/web.rs`) need no crate: the WebSocket client is in the file.
  `open --web` adds `--remote-debugging-port` to the command; the port is read from the
  profile folder (`WebDriverBiDiServer.json` of Firefox, `DevToolsActivePort` of Chromium), so
  `vwm web` needs the profile wirbel made, not one given with the browser's own option.
  Firefox takes one BiDi session at a time: each command ends its session (`Drop for Page`).
  Tried with Firefox and Chromium: goto, title, eval, count, click, fill.
- `vwm web` always acts on the first tab.
- Measured with an invisible terminal: the window itself costs about 60 MB (hidden wirbel 29,
  Xwayland 26, session bus and portal 5; shared pages counted by their share), the rest is the
  program. Starting Xwayland only when an X11 program connects would save a good part of it.
- Try by hand without touching a running desktop: start `wirbel --headless --log FILE`, read
  the socket name from the log, set `WAYLAND_DISPLAY` to it, then `wirbel vwm ...`.

## Checklist for the first try on a real desktop

After installing the new binary and restarting wirbel:

- `wirbel vwm open --name t kitty`, nothing shows, no focus change, the taskbar is unchanged.
- `wirbel vwm list` shows `t`; `wirbel vwm type t 'echo hi'`, `wirbel vwm key t ENTER`,
  `wirbel vwm shot t /tmp/t.png` shows the line.
- `wirbel macro run examples/macros/page-title.wm` prints the title of example.com.
- `wirbel vwm adopt focused x` hides the focused window, `wirbel vwm release x` brings it back.
- Sound: play something in an invisible browser, nothing is heard; `wirbel vwm audio NAME
  listen on` makes it heard.
- Logging out or `wirbel restart` leaves no `wirbel --shadow` process behind
  (`pgrep -af -- --shadow`) and no sink (`pactl list short sinks | grep wirbel-vwm`).

## First try on the real desktop (2026-11-18)

- Passed: invisible kitty (nothing shows, no focus change), typing incl. the German layout and
  CTRL, shot, adopt focused + shot + release, sound silent in its sink and heard with
  `audio NAME listen on`, closing leaves no shadow and no sink.
- Fixed on the way: fresh browser profiles opened first-run pages that took the keyboard, so
  the example macro returned "Welcome to Firefox" (99ad1da); a headless wirbel shared the state
  folders of the desktop and swept the sink of a desktop window (65f87a2, now `vwm/DISPLAY/NAME`).
- Checked after `wirbel restart` with the new build (19:20): no `--shadow` process and no `wirbel-vwm` sink
  left behind; the example macro now returns "Example Domain — Mozilla Firefox".
