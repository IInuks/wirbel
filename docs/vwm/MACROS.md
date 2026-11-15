# Macros

A macro is a text file of steps that drive invisible windows. One step per line, `#` starts a
comment. Run one with `wirbel macro run FILE [NAME=VALUE]...`, check one without running it with
`wirbel macro check FILE`. Files in `~/.config/wirbel/macros` can be named without folder and
without `.wm`; `wirbel macro list` shows them. To run one from a key, bind
`spawn, wirbel macro run NAME`.

```
param url = "https://example.com"

open page = firefox
wait page ready
key page CTRL+l
type page "$url"
key page Return
wait page stable 800 timeout 20s

try
    let where = find page logo.png
else
    fail "the logo is not there: $error"
end
print "logo at $where"
close page
```

## Steps for invisible windows

Every command of `wirbel vwm` is a step with the same words: `open`, `close`, `type`, `key`,
`click`, `move`, `drag`, `scroll`, `shot`, `clip`, `paste`, `resize`, `status`, `pixel`, `find`,
`text`, `click-text`, `ui`, `audio`, `adopt`, `release`, `windows`, `list`, `info`. `wirbel vwm --help` describes them.

- `open NAME = COMMAND...` starts a program and names its window. `open COMMAND...` picks a name
  and yields it: `let w = open kitty`. Options go first: `open page = --size 800x600 firefox`.
- `wait NAME CONDITION [timeout TIME]` waits until the condition holds and yields what it
  answers. Conditions: `ready`, `window [GLOB]`, `title GLOB`, `closed`, `exited`, `stable [MS]`,
  `changed [X,Y,W,H]`, `pixel X Y #RRGGBB [TOLERANCE]`, `image FILE.png [SIMILARITY]`,
  `text GLOB`, `loaded`, `sound`, `silence [MS]`, `cursor NAME`, `clipboard`, `file PATH`. When the time runs out the step fails; the limit is
  30 seconds unless the step or an earlier `timeout TIME` says otherwise.

## Words

The words of a step are separated by spaces.

- `"text with $name"` is one word; `$name` and `${name}` are replaced by the variable, `$$` is a
  dollar, `\n \t \" \\` work.
- `'text'` is one word exactly as written.
- `(expression)` is the value of the expression.
- `name(values)` calls a function, `$list[0]` takes an item.
- Anything else is a word as it stands, with `$name` replaced: `--size`, `CTRL+l`, `pre$name`.

A time is `20s`, `500ms`, `2m` or a number of seconds.

## Values and variables

Values are texts, numbers, `true`/`false`, `nil` and lists `[1, "two", $three]`.

- `let NAME = VALUE` makes a variable, `set NAME = VALUE` changes one. Inside a function `let`
  makes a variable of that call.
- VALUE is an expression, or a step that yields something: a `vwm` command (what it prints),
  `wait`, `shell COMMAND` (what it prints; fails if the command fails), `ask QUESTION` (a line
  typed in the terminal), `read FILE`.
- `param NAME = DEFAULT` is a variable that `NAME=VALUE` on the command line overrides;
  `param NAME` has to be given.

Expressions: `+ - * / %`, `== != < <= > >=`, `and or not`, `TEXT matches GLOB`,
`A contains B`, `A in B`, brackets. Variables are always written `$name`; a bare word is a text.
Texts that read as numbers compare and calculate as numbers. `+` joins texts and lists.
Empty text, `0`, `false`, `nil` and an empty list count as false.

Functions: `len`, `num`, `str`, `int`, `round`, `trim`, `upper`, `lower`, `lines`, `words`,
`split(text, by)`, `join(list, with)`, `replace(text, from, to)`, `starts(text, start)`,
`ends(text, end)`, `find(text, part)` (position or -1), `slice(value, from, to)`,
`range(from, to)`, `min`, `max`, `env(name)`, `exists(path)`, `now()`.

## Flow

```
if $n > 3            while $n < 10          for item in $list       repeat 5
    ...                  ...                    ...                     ...
elif $n == 3         end                    end                     end
    ...
else
    ...
end
```

`break` and `continue` work in all loops. `for` goes through a list, or through the lines of a
text.

```
func greet(name)
    print "hello $name"
    return upper($name)
end
let loud = greet(world)
greet(again)
```

`use FILE` runs another macro file once, for its functions and variables. It is looked for next
to the macro and in `~/.config/wirbel/macros`.

## Errors and endings

```
try
    wait page title "*Done*" timeout 5s
else
    print "gave up: $error"
end
```

A step that fails ends the macro with `FILE:LINE: message` unless it is inside `try`. `try`
without `else` only swallows the failure. `fail TEXT` fails on purpose.

`return VALUE` at the top ends the macro and prints the value (`--json` prints it as JSON).
`exit [CODE]` ends it from anywhere. The exit status is 0, 1 after a failure, 3 after a wait
that ran out, or the code of `exit`.

## Reacting to what happens

```
on page title "*Cookies*" do
    key page Escape
end
```

A handler runs its steps when its condition comes true: while the macro waits or sleeps and
between its steps, at most four times a second. It runs once each time the condition appears,
not as long as it stays. Conditions are those of `wait`, except `changed` and `clipboard`.
`off NAME` removes the handlers of a window, `off` alone all of them.

## Other steps

`print WORDS`, `sleep TIME`, `timeout TIME`, `shell COMMAND`, `ask QUESTION`, `read FILE`,
`write FILE WORDS`, `append FILE WORDS`.
