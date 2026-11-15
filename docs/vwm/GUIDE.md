# Invisible windows: a guide

`wirbel vwm` runs programs in windows that never show on the screen. They do not appear on any
workspace or in the taskbar, never take the focus, and are driven by commands: typing, clicking,
taking pictures, reading text, waiting for something to happen. Each one lives in a hidden
wirbel of its own, so it has its own focus and clipboard and cannot disturb what is on screen.

## First steps

```sh
wirbel vwm open --name page firefox      # prints the name: page
wirbel vwm wait page ready               # until its window has drawn
wirbel vwm key page CTRL+l
wirbel vwm type page "https://example.com"
wirbel vwm key page ENTER
wirbel vwm wait page stable 800          # the picture rests for 0.8 s
wirbel vwm shot page page.png            # a picture of it
wirbel vwm status page                   # title, windows, cursor ...
wirbel vwm list
wirbel vwm close page
```

Positions are pixels from the top left corner of the window. The window is 1280x800 unless
`--size WxH` says otherwise; `wirbel vwm resize NAME WxH` changes it later.

## What there is

| Command | Does |
|---|---|
| `open [--name N] [--size WxH] [--share-bus] [--env K=V] [--] COMMAND` | start a program invisibly |
| `list`, `info NAME`, `status NAME`, `events NAME` | look at them |
| `close NAME...\|all [--kill]` | close politely, or at once |
| `windows`, `adopt WINDOW\|focused [NAME]`, `release NAME` | park a window that is already open, bring it back |
| `type NAME TEXT`, `key NAME KEY...`, `paste NAME TEXT` | keyboard; `paste` takes any text |
| `click NAME X Y [BUTTON] [COUNT]`, `move`, `drag`, `scroll` | pointer |
| `shot NAME [FILE] [--region X,Y,W,H] [--cursor]`, `pixel NAME X Y` | pictures |
| `find NAME FILE.png [SIMILARITY]` | where a small picture shows in the window |
| `clip NAME get\|set TEXT` | the window's own clipboard |
| `text NAME [--via a11y\|ocr\|clipboard] [--find GLOB] [--where]`, `click-text NAME GLOB` | read text, click on text |
| `ui NAME tree`, `ui NAME press LABEL`, `ui NAME set LABEL TEXT` | controls through the accessibility interface |
| `audio NAME listen on\|off`, `audio NAME level`, `audio NAME record FILE.wav [S]` | its sound |
| `record NAME FILE [--fps N] [--audio] [S]` | film it with ffmpeg (.mp4, .webm, .gif) |
| `web NAME goto URL`, `web NAME text [SELECTOR]`, `web NAME click SELECTOR`, `web NAME fill SELECTOR TEXT`, `web NAME eval SCRIPT` | the page of a browser opened with `open --web` |
| `wait NAME CONDITION [--timeout S]` | wait for a state |

`wirbel vwm --help` has every option. A wait whose time runs out exits with status 3, so shell
scripts can tell it from a failure (1).

Wait conditions: `ready`, `window [GLOB]`, `title GLOB`, `closed`, `exited`, `stable [MS]`,
`changed [X,Y,W,H]`, `pixel X Y #RRGGBB`, `image FILE.png`, `text GLOB`, `loaded`, `cursor NAME`,
`clipboard`, `file PATH`, `sound`, `silence [MS]`.

## Good to know

- An invisible window is silent: its program plays into a sink of its own. `audio NAME listen
  on` lets you hear it, `open --sound` starts a program that plays on the speakers as usual.
- Typing uses the keyboard layout of the desktop, including AltGr. Text the layout has no key
  for (other scripts, emoji) goes through `paste`. Terminals paste with
  `paste NAME --key CTRL+SHIFT+v TEXT`.
- A program gets a D-Bus session bus of its own, so that it starts a new instance instead of
  handing over to a visible one. `--share-bus` gives it the bus of the desktop, for programs
  that need its services (keyring, notifications).
- Browsers get a fresh profile in `~/.local/state/wirbel/vwm/NAME/profile`. It stays there, so
  the same name finds its logins again. To use another profile pass the browser's own option:
  `wirbel vwm open -- firefox --profile DIR`.
- What the hidden wirbel and the program print is in `~/.local/state/wirbel/vwm/NAME/shadow.log`.
- Invisible windows end with the desktop: logging out or `wirbel restart` closes them.
- `text --via a11y` and `ui` ask the program itself (GTK, Qt, browsers): exact text, controls
  by name, and `wait NAME loaded` knows when a page has loaded.
- Reading text from a picture needs the program `tesseract` installed (and a language pack);
  without it `text --via clipboard` still works where a program has select all and copy.
- Invisible windows are rendered by `vwm_gpu` of the configuration; without it by `render_gpu`,
  else by a card that is not driven by nvidia if the machine has several.
- Each invisible window costs about 60 MB besides its program (a hidden wirbel, an X server
  and a session bus of its own); `wirbel vwm list` shows the memory of each. Nothing is drawn
  while the program does not change its picture.

## Macros

Several steps with conditions, loops and error handling go in a macro file, see
[MACROS.md](MACROS.md):

```sh
wirbel macro run examples/macros/page-title.wm url=https://example.com
```

## Windows that are already open

A running program cannot move into a hidden wirbel, so `adopt` parks its window instead: it
leaves its workspace and the taskbar and keeps running out of sight, `release` brings it back.
A parked window can be photographed (`shot`), listed and closed. It cannot be typed into or
clicked: on the desktop there is one keyboard and one pointer, and they stay with you. Start a
program with `open` when it is to be driven.

```sh
wirbel vwm adopt focused music     # the window with the focus disappears
wirbel vwm shot music now.png
wirbel vwm release music
```

## Browsers

`open --web` starts Firefox or a Chromium with its remote control switched on, and `web` talks
to the page itself: exact text, elements by CSS selector, JavaScript.

```sh
wirbel vwm open --name shop --web firefox
wirbel vwm wait shop ready
wirbel vwm web shop goto https://example.com      # returns when the page has loaded
wirbel vwm web shop text h1
wirbel vwm web shop fill 'input[name=q]' rust
wirbel vwm web shop click 'button[type=submit]'
wirbel vwm wait shop loaded
wirbel vwm web shop eval 'document.links.length'
```

The remote control is a port on this machine that any program of this machine can use to
steer that browser, so it is only on where `--web` asks for it. Without it a browser is driven
like any window: keys, clicks, `text`, `ui`.

## Not there yet

Input into parked windows and a window to look into an invisible one live are planned (until
then `shot` and `record` show what it looks like);
[PROGRESS.md](PROGRESS.md) says how far they are.
