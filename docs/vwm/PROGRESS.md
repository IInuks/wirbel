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

Phase 1, step 1.1.

## Phases

### 0 Docs
- [x] 0.1 RESEARCH.md, DESIGN.md, PROGRESS.md

### 1 Shadow core
- [ ] 1.1 `--shadow NAME [WxH]` start mode: headless, injection on, no config, no binds, no
      borders or gaps, named control socket
- [ ] 1.2 single layout in a shadow: first window fills the output, later ones float centred
- [ ] 1.3 manager in the main compositor: registry, start, supervision, cleanup at exit
- [ ] 1.4 requests `vwm open|list|info|close|resize|where` on the control socket
- [ ] 1.5 command line `wirbel vwm open|list|info|close|resize` and the `invisible` aliases
- [ ] 1.6 private D-Bus session bus per shadow, `--share-bus`, `--env`
- [ ] 1.7 program recipes (Firefox, Chromium family) with profiles in the state folder
- [ ] 1.8 integration tests `tests/vwm.rs`

### 2 Control
- [ ] 2.1 absolute pointer requests: move, click, drag, scroll
- [ ] 2.2 `capture` request writing a PNG
- [ ] 2.3 clipboard get and set
- [ ] 2.4 command line `vwm type|key|click|move|drag|scroll|shot|clip`
- [ ] 2.5 tests

### 3 Events and waiting
- [ ] 3.1 shadow events: damage, client exit, cursor, clipboard
- [ ] 3.2 wait engine with deadlines; conditions ready, window, title, closed, exited, stable,
      changed, pixel, image, file
- [ ] 3.3 command line `vwm wait|events`
- [ ] 3.4 tests

### 4 Macros
- [ ] 4.1 lexer and parser with line numbers
- [ ] 4.2 interpreter: values, variables, expressions, control flow, functions, try
- [ ] 4.3 vwm steps, `on` handlers, timeouts
- [ ] 4.4 `wirbel macro run|check|list`, bind action `macro`
- [ ] 4.5 MACROS.md, examples, tests

### 5 Text I
- [ ] 5.1 clipboard trick
- [ ] 5.2 OCR through `tesseract`, positions, `wait text`

### 6 Text II
- [ ] 6.1 AT-SPI: tree, text, press, set
- [ ] 6.2 AT-SPI signals, `loaded`

### 7 Adoption
- [ ] 7.1 parked list, hidden from taskbar, frame callbacks
- [ ] 7.2 offscreen capture
- [ ] 7.3 `adopt`, `release`, focus borrowing behind `vwm_adopt_input`

### 8 Audio
- [ ] 8.1 null sink per instance, `record`, `listen`, `level`, sound and silence events

### 9 Video
- [ ] 9.1 `record` through `ffmpeg`, frame export

### 10 Browser protocol
- [ ] 10.1 `vwm web`, `loaded` and `text` through it

### 11 Viewer and finish
- [ ] 11.1 `vwm show|hide`
- [ ] 11.2 limits, memory column, README, GUIDE.md

## Notes and known problems

(none yet)
