//! Running a macro.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use super::syntax::{Block, Expr, Kind, Op, Piece, Source, Step, parse};
use crate::vwm::wait::{Condition, DEFAULT_TIMEOUT, WaitError, glob};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Num(f64),
    Text(String),
    List(Vec<Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nil => Ok(()),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Num(value) if value.fract() == 0.0 && value.abs() < 1e15 => {
                write!(f, "{}", *value as i64)
            }
            Self::Num(value) => write!(f, "{value}"),
            Self::Text(text) => f.write_str(text),
            Self::List(items) => {
                let items: Vec<String> = items.iter().map(Value::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
        }
    }
}

impl Value {
    pub fn truthy(&self) -> bool {
        match self {
            Self::Nil => false,
            Self::Bool(value) => *value,
            Self::Num(value) => *value != 0.0,
            Self::Text(text) => !text.is_empty(),
            Self::List(items) => !items.is_empty(),
        }
    }

    /// As a number, if it is one or text that reads as one.
    pub fn number(&self) -> Option<f64> {
        match self {
            Self::Num(value) => Some(*value),
            Self::Text(text) => text.trim().parse().ok().filter(|n: &f64| n.is_finite()),
            Self::Bool(value) => Some(f64::from(u8::from(*value))),
            _ => None,
        }
    }

    pub fn to_json(&self) -> String {
        match self {
            Self::Nil => "null".into(),
            Self::Bool(_) | Self::Num(_) => self.to_string(),
            Self::Text(text) => crate::ipc::json_string(text),
            Self::List(items) => {
                let items: Vec<String> = items.iter().map(Value::to_json).collect();
                format!("[{}]", items.join(","))
            }
        }
    }
}

/// `20s`, `500ms`, `2m`, or a number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1.0)
    } else if let Some(number) = text.strip_suffix('m') {
        (number, 60.0)
    } else {
        (text, 1.0)
    };
    let seconds = number.trim().parse::<f64>().ok().map(|n| n * scale);
    seconds
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| format!("expected a time such as 20s, 500ms or 2m, got `{text}`"))
}

/// What a macro does to the world outside of itself. The tests have their own.
pub trait Host {
    /// A command for invisible windows, as `wirbel vwm` takes it. Returns what it prints.
    fn command(&mut self, args: &[String]) -> Result<String, String>;
    /// Waits for a condition; `tick` runs between two looks with this host.
    fn wait(
        &mut self,
        name: &str,
        condition: &Condition,
        timeout: Duration,
        tick: &mut dyn FnMut(&mut dyn Host) -> Result<(), String>,
    ) -> Result<String, WaitError>;
    fn print(&mut self, line: &str);
    fn ask(&mut self, prompt: &str) -> Result<String, String>;
    fn shell(&mut self, command: &str) -> Result<String, String>;
    fn sleep(&mut self, time: Duration);
    fn read(&mut self, path: &Path) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub file: String,
    pub line: usize,
    pub message: String,
    /// Whether the time of a wait ran out.
    pub timeout: bool,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

/// How a block ended.
enum Flow {
    Next,
    Break,
    Continue,
    Return(Value),
    Exit(i32),
}

/// How a macro ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// With this value, `Nil` if it returned nothing.
    Done(Value),
    Exit(i32),
}

struct Function {
    params: Vec<String>,
    body: Block,
    file: Rc<str>,
}

struct Handler {
    target: String,
    condition: Condition,
    body: Block,
    file: Rc<str>,
    /// Whether the condition was seen not to hold since the handler last ran: it runs when
    /// something appears, not as long as it is there.
    armed: bool,
}

/// How often the handlers look.
const HANDLER_PERIOD: Duration = Duration::from_millis(250);
/// Deeper calls than this are a function that calls itself without end.
const MAX_DEPTH: usize = 200;

pub struct Interp<'a> {
    host: &'a mut dyn Host,
    /// The words that are commands for invisible windows.
    commands: &'a [&'a str],
    globals: HashMap<String, Value>,
    locals: Vec<HashMap<String, Value>>,
    functions: HashMap<String, Rc<Function>>,
    handlers: Vec<Handler>,
    in_handler: bool,
    handlers_ran: Option<Instant>,
    timeout: Duration,
    /// Values for `param` from the command line.
    given: HashMap<String, String>,
    file: Rc<str>,
    /// The files a `use` is inside of.
    using: Vec<PathBuf>,
    /// Where `use` looks besides the folder of the macro.
    pub library: Option<PathBuf>,
}

type Res<T> = Result<T, Error>;

impl<'a> Interp<'a> {
    pub fn new(
        host: &'a mut dyn Host,
        commands: &'a [&'a str],
        given: HashMap<String, String>,
    ) -> Self {
        Self {
            host,
            commands,
            globals: HashMap::new(),
            locals: Vec::new(),
            functions: HashMap::new(),
            handlers: Vec::new(),
            in_handler: false,
            handlers_ran: None,
            timeout: DEFAULT_TIMEOUT,
            given,
            file: "macro".into(),
            using: Vec::new(),
            library: None,
        }
    }

    /// Runs the macro in `source`, which was read from `file`.
    pub fn run(&mut self, source: &str, file: &Path) -> Res<Outcome> {
        self.file = file.display().to_string().into();
        self.using.push(file.to_path_buf());
        let steps = parse(source, self.commands).map_err(|err| Error {
            file: self.file.to_string(),
            line: err.line,
            message: err.message,
            timeout: false,
        })?;
        let flow = match self.block(&steps) {
            Err(err) if err.file == EXIT => Flow::Exit(err.line as i32),
            flow => flow?,
        };
        if let Some(unknown) = self.given.keys().next() {
            return Err(self.error(0, format!("the macro has no parameter `{unknown}`")));
        }
        Ok(match flow {
            Flow::Exit(code) => Outcome::Exit(code),
            Flow::Return(value) => Outcome::Done(value),
            _ => Outcome::Done(Value::Nil),
        })
    }

    fn error(&self, line: usize, message: impl Into<String>) -> Error {
        Error {
            file: self.file.to_string(),
            line,
            message: message.into(),
            timeout: false,
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        let local = self.locals.last().and_then(|frame| frame.get(name));
        local.or_else(|| self.globals.get(name))
    }

    fn define(&mut self, name: &str, value: Value) {
        let frame = self.locals.last_mut().unwrap_or(&mut self.globals);
        frame.insert(name.to_string(), value);
    }

    fn block(&mut self, steps: &[Step]) -> Res<Flow> {
        for step in steps {
            match self.step(step)? {
                Flow::Next => {}
                flow => return Ok(flow),
            }
        }
        Ok(Flow::Next)
    }

    /// The words of a step as text.
    fn texts(&mut self, words: &[Expr], line: usize) -> Res<Vec<String>> {
        let values: Res<Vec<Value>> = words.iter().map(|word| self.eval(word, line)).collect();
        Ok(values?.iter().map(Value::to_string).collect())
    }

    /// One word keeps its kind, several are joined to a text.
    fn joined(&mut self, words: &[Expr], line: usize) -> Res<Value> {
        match words {
            [] => Ok(Value::Nil),
            [word] => self.eval(word, line),
            words => Ok(Value::Text(self.texts(words, line)?.join(" "))),
        }
    }

    fn source(&mut self, source: &Source, line: usize) -> Res<Value> {
        match source {
            Source::Expr(expr) => self.eval(expr, line),
            Source::Step(step) => self.value_step(step),
        }
    }

    fn duration(&mut self, expr: &Expr, line: usize) -> Res<Duration> {
        let value = self.eval(expr, line)?;
        parse_duration(&value.to_string()).map_err(|err| self.error(line, err))
    }

    /// The steps that yield a value.
    fn value_step(&mut self, step: &Step) -> Res<Value> {
        let line = step.line;
        let text = |text: String| Value::Text(text);
        match &step.kind {
            Kind::Command(words) => {
                self.run_handlers(line)?;
                let mut args = self.texts(words, line)?;
                // `open NAME = COMMAND` names the window.
                if args.len() > 3 && args[0] == "open" && args[2] == "=" {
                    let name = args.remove(1);
                    args.remove(1);
                    args.splice(1..1, ["--name".to_string(), name]);
                }
                let answer = self.host.command(&args);
                answer.map(text).map_err(|err| self.error(line, err))
            }
            Kind::Wait(words) => {
                let mut words = self.texts(words, line)?;
                let mut timeout = self.timeout;
                if let [.., word, time] = words.as_slice()
                    && word == "timeout"
                {
                    timeout = parse_duration(time).map_err(|err| self.error(line, err))?;
                    words.truncate(words.len() - 2);
                }
                let Some((name, condition)) = words.split_first() else {
                    return Err(self.error(line, "expected `wait NAME CONDITION`"));
                };
                let condition: Vec<&str> = condition.iter().map(String::as_str).collect();
                let condition =
                    Condition::parse(&condition).map_err(|err| self.error(line, err))?;
                self.wait(name, &condition, timeout, line).map(text)
            }
            Kind::Shell(words) => {
                let command = self.texts(words, line)?.join(" ");
                let answer = self.host.shell(&command);
                answer.map(text).map_err(|err| self.error(line, err))
            }
            Kind::Ask(words) => {
                let prompt = self.texts(words, line)?.join(" ");
                let answer = self.host.ask(&prompt);
                answer.map(text).map_err(|err| self.error(line, err))
            }
            Kind::Read(words) => {
                let path = self.texts(words, line)?.join(" ");
                let answer = self.host.read(Path::new(&path));
                answer.map(text).map_err(|err| self.error(line, err))
            }
            _ => Err(self.error(line, "this step has no value")),
        }
    }

    /// Waits, running the handlers meanwhile.
    fn wait(
        &mut self,
        name: &str,
        condition: &Condition,
        timeout: Duration,
        line: usize,
    ) -> Res<String> {
        // The handlers need the interpreter, the wait needs the host: the host hands itself
        // to the tick, and the tick borrows the interpreter's state through this pointer-free
        // detour of taking the handlers out.
        let mut handlers = std::mem::take(&mut self.handlers);
        let mut fired: Vec<usize> = Vec::new();
        let mut failure = None;
        let in_handler = self.in_handler;
        let mut last = Instant::now();
        let result = loop {
            let mut tick = |host: &mut dyn Host| -> Result<(), String> {
                if in_handler || last.elapsed() < HANDLER_PERIOD {
                    return Ok(());
                }
                last = Instant::now();
                for (index, handler) in handlers.iter_mut().enumerate() {
                    let met = host
                        .wait(
                            &handler.target,
                            &handler.condition,
                            Duration::ZERO,
                            &mut |_| Ok(()),
                        )
                        .is_ok();
                    if met && handler.armed {
                        handler.armed = false;
                        fired.push(index);
                    } else if !met {
                        handler.armed = true;
                    }
                }
                // Leave the wait to run what fired; it begins again after.
                match fired.is_empty() {
                    true => Ok(()),
                    false => Err(String::new()),
                }
            };
            let began = Instant::now();
            let result = self.host.wait(name, condition, timeout, &mut tick);
            if fired.is_empty() {
                break result;
            }
            self.handlers = std::mem::take(&mut handlers);
            for index in std::mem::take(&mut fired) {
                let (body, file) = (
                    self.handlers[index].body.clone(),
                    self.handlers[index].file.clone(),
                );
                if let Err(err) = self.handler_body(&body, file) {
                    failure = Some(err);
                    break;
                }
            }
            handlers = std::mem::take(&mut self.handlers);
            if failure.is_some() {
                break Ok(String::new());
            }
            // What the handler took is taken from the wait, too: the limit is for the whole.
            let _ = began;
        };
        self.handlers = handlers;
        if let Some(err) = failure {
            return Err(err);
        }
        result.map_err(|err| Error {
            timeout: matches!(err, WaitError::Timeout(_)),
            ..self.error(line, err.message())
        })
    }

    fn handler_body(&mut self, body: &Block, file: Rc<str>) -> Res<()> {
        let outer = std::mem::replace(&mut self.file, file);
        self.in_handler = true;
        let locals = std::mem::take(&mut self.locals);
        let flow = self.block(body);
        self.locals = locals;
        self.in_handler = false;
        self.file = outer;
        flow.map(drop)
    }

    /// Looks whether a handler is due, outside of waits.
    fn run_handlers(&mut self, line: usize) -> Res<()> {
        let due = self
            .handlers_ran
            .is_none_or(|ran| ran.elapsed() >= HANDLER_PERIOD);
        if self.in_handler || self.handlers.is_empty() || !due {
            return Ok(());
        }
        self.handlers_ran = Some(Instant::now());
        let _ = line;
        for index in 0..self.handlers.len() {
            let handler = &self.handlers[index];
            let met = self
                .host
                .wait(
                    &handler.target,
                    &handler.condition,
                    Duration::ZERO,
                    &mut |_| Ok(()),
                )
                .is_ok();
            let handler = &mut self.handlers[index];
            if met && handler.armed {
                handler.armed = false;
                let (body, file) = (handler.body.clone(), handler.file.clone());
                self.handler_body(&body, file)?;
            } else if !met {
                handler.armed = true;
            }
        }
        Ok(())
    }

    fn step(&mut self, step: &Step) -> Res<Flow> {
        let line = step.line;
        match &step.kind {
            Kind::Let(name, source) => {
                let value = self.source(source, line)?;
                self.define(name, value);
            }
            Kind::Set(name, source) => {
                let value = self.source(source, line)?;
                let local = self
                    .locals
                    .last_mut()
                    .filter(|frame| frame.contains_key(name));
                let frame = match local {
                    Some(frame) => frame,
                    None if self.globals.contains_key(name) => &mut self.globals,
                    None => {
                        return Err(
                            self.error(line, format!("no variable `{name}`, `let` makes one"))
                        );
                    }
                };
                frame.insert(name.clone(), value);
            }
            Kind::Param(name, default) => {
                let value = match (self.given.remove(name), default) {
                    (Some(given), _) => Value::Text(given),
                    (None, Some(default)) => self.source(default, line)?,
                    (None, None) => {
                        return Err(self.error(line, format!("the macro needs `{name}=VALUE`")));
                    }
                };
                self.globals.insert(name.clone(), value);
            }
            Kind::Print(words) => {
                let line_out = self.texts(words, line)?.join(" ");
                self.host.print(&line_out);
            }
            Kind::If(branches, otherwise) => {
                for (condition, body) in branches {
                    if self.eval(condition, line)?.truthy() {
                        return self.block(body);
                    }
                }
                if let Some(body) = otherwise {
                    return self.block(body);
                }
            }
            Kind::While(condition, body) => {
                while self.eval(condition, line)?.truthy() {
                    match self.block(body)? {
                        Flow::Break => break,
                        Flow::Next | Flow::Continue => {}
                        flow => return Ok(flow),
                    }
                    self.run_handlers(line)?;
                }
            }
            Kind::For(name, list, body) => {
                let items = match self.eval(list, line)? {
                    Value::List(items) => items,
                    Value::Text(text) => text.lines().map(|l| Value::Text(l.to_string())).collect(),
                    Value::Nil => Vec::new(),
                    other => return Err(self.error(line, format!("cannot go through `{other}`"))),
                };
                for item in items {
                    self.define(name, item);
                    match self.block(body)? {
                        Flow::Break => break,
                        Flow::Next | Flow::Continue => {}
                        flow => return Ok(flow),
                    }
                }
            }
            Kind::Repeat(count, body) => {
                let count = self.eval(count, line)?;
                let count = count.number().filter(|n| *n >= 0.0);
                let count = count.ok_or_else(|| self.error(line, "`repeat` needs a number"))?;
                for _ in 0..count as u64 {
                    match self.block(body)? {
                        Flow::Break => break,
                        Flow::Next | Flow::Continue => {}
                        flow => return Ok(flow),
                    }
                }
            }
            Kind::Func(name, params, body) => {
                let function = Function {
                    params: params.clone(),
                    body: body.clone(),
                    file: self.file.clone(),
                };
                self.functions.insert(name.clone(), Rc::new(function));
            }
            Kind::Return(words) => return Ok(Flow::Return(self.joined(words, line)?)),
            Kind::Try(body, otherwise) => match self.block(body) {
                Ok(flow) => return Ok(flow),
                // An `exit` on its way out of a function is no failure to catch.
                Err(err) if err.file == EXIT => return Err(err),
                Err(err) => {
                    if let Some(otherwise) = otherwise {
                        self.define("error", Value::Text(err.message));
                        return self.block(otherwise);
                    }
                }
            },
            Kind::Fail(words) => {
                let message = self.texts(words, line)?.join(" ");
                let message = if message.is_empty() {
                    "failed".into()
                } else {
                    message
                };
                return Err(self.error(line, message));
            }
            Kind::Exit(code) => {
                let code = match code {
                    Some(code) => self.eval(code, line)?.number().unwrap_or(1.0) as i32,
                    None => 0,
                };
                return Ok(Flow::Exit(code));
            }
            Kind::Sleep(time) => {
                let mut left = self.duration(time, line)?;
                // In pieces, so that the handlers get their turn.
                while !left.is_zero() {
                    let piece = left.min(HANDLER_PERIOD);
                    self.host.sleep(piece);
                    left -= piece;
                    self.run_handlers(line)?;
                }
            }
            Kind::Timeout(time) => self.timeout = self.duration(time, line)?,
            Kind::Use(file) => {
                let name = self.eval(file, line)?.to_string();
                self.use_file(&name, line)?;
            }
            Kind::On(head, body) => {
                let head = self.texts(head, line)?;
                let words: Vec<&str> = head[1..].iter().map(String::as_str).collect();
                let condition = Condition::parse(&words).map_err(|err| self.error(line, err))?;
                if matches!(condition, Condition::Changed(_) | Condition::Clipboard) {
                    return Err(
                        self.error(line, "`on` takes no condition that compares with before")
                    );
                }
                self.handlers.push(Handler {
                    target: head[0].clone(),
                    condition,
                    body: body.clone(),
                    file: self.file.clone(),
                    armed: true,
                });
            }
            Kind::Off(words) => {
                let names = self.texts(words, line)?;
                self.handlers
                    .retain(|handler| !names.is_empty() && !names.contains(&handler.target));
            }
            Kind::Break => return Ok(Flow::Break),
            Kind::Continue => return Ok(Flow::Continue),
            Kind::Write(append, words) => {
                let words = self.texts(words, line)?;
                let (path, text) = words.split_first().expect("the parser wants a word");
                let text = text.join(" ") + "\n";
                let written = if *append {
                    use std::io::Write;
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path);
                    file.and_then(|mut file| file.write_all(text.as_bytes()))
                } else {
                    std::fs::write(path, text)
                };
                written.map_err(|err| self.error(line, format!("{path}: {err}")))?;
            }
            Kind::Call(call) => {
                self.eval(call, line)?;
            }
            Kind::Command(_) | Kind::Wait(_) | Kind::Shell(_) | Kind::Ask(_) | Kind::Read(_) => {
                self.value_step(step)?;
            }
        }
        Ok(Flow::Next)
    }

    fn use_file(&mut self, name: &str, line: usize) -> Res<()> {
        let here = self.using.last().and_then(|file| file.parent());
        let places = [here.map(Path::to_path_buf), self.library.clone()];
        let found = places
            .into_iter()
            .flatten()
            .flat_map(|dir| [dir.join(name), dir.join(format!("{name}.wm"))]);
        let path = found.into_iter().find(|path| path.is_file());
        let path = path.ok_or_else(|| self.error(line, format!("no macro file `{name}`")))?;
        if self.using.contains(&path) {
            return Ok(());
        }
        let source = self.host.read(&path).map_err(|err| self.error(line, err))?;
        let outer = std::mem::replace(&mut self.file, path.display().to_string().into());
        let steps = parse(&source, self.commands).map_err(|err| Error {
            file: self.file.to_string(),
            line: err.line,
            message: err.message,
            timeout: false,
        })?;
        self.using.push(path);
        let flow = self.block(&steps);
        self.using.pop();
        self.file = outer;
        flow.map(drop)
    }

    fn eval(&mut self, expr: &Expr, line: usize) -> Res<Value> {
        Ok(match expr {
            Expr::Nil => Value::Nil,
            Expr::Bool(value) => Value::Bool(*value),
            Expr::Num(value) => Value::Num(*value),
            Expr::Text(pieces) => {
                let mut text = String::new();
                for piece in pieces {
                    match piece {
                        Piece::Lit(lit) => text.push_str(lit),
                        Piece::Var(name) => text.push_str(&self.variable(name, line)?.to_string()),
                    }
                }
                Value::Text(text)
            }
            Expr::Var(name) => self.variable(name, line)?,
            Expr::List(items) => {
                let items: Res<Vec<Value>> =
                    items.iter().map(|item| self.eval(item, line)).collect();
                Value::List(items?)
            }
            Expr::Not(inner) => Value::Bool(!self.eval(inner, line)?.truthy()),
            Expr::Neg(inner) => {
                let value = self.eval(inner, line)?;
                let number = value.number();
                Value::Num(
                    -number.ok_or_else(|| self.error(line, format!("`{value}` is no number")))?,
                )
            }
            Expr::Binary(left, Op::And, right) => {
                let left = self.eval(left, line)?;
                if !left.truthy() {
                    return Ok(left);
                }
                self.eval(right, line)?
            }
            Expr::Binary(left, Op::Or, right) => {
                let left = self.eval(left, line)?;
                if left.truthy() {
                    return Ok(left);
                }
                self.eval(right, line)?
            }
            Expr::Binary(left, op, right) => {
                let (left, right) = (self.eval(left, line)?, self.eval(right, line)?);
                binary(&left, *op, &right).map_err(|err| self.error(line, err))?
            }
            Expr::Index(list, index) => {
                let (list, index) = (self.eval(list, line)?, self.eval(index, line)?);
                let at = index.number().map(|n| n as i64);
                let at = at.ok_or_else(|| self.error(line, format!("`{index}` is no position")))?;
                let pick = |length: usize| match at {
                    at if at >= 0 => Some(at as usize).filter(|at| *at < length),
                    at => length.checked_sub(at.unsigned_abs() as usize),
                };
                match &list {
                    Value::List(items) => {
                        pick(items.len()).map_or(Value::Nil, |at| items[at].clone())
                    }
                    Value::Text(text) => {
                        let chars: Vec<char> = text.chars().collect();
                        pick(chars.len())
                            .map_or(Value::Nil, |at| Value::Text(chars[at].to_string()))
                    }
                    other => return Err(self.error(line, format!("`{other}` has no positions"))),
                }
            }
            Expr::Call(name, args) => {
                let values: Res<Vec<Value>> = args.iter().map(|arg| self.eval(arg, line)).collect();
                self.call(name, values?, line)?
            }
        })
    }

    fn variable(&self, name: &str, line: usize) -> Res<Value> {
        let value = self.get(name).cloned();
        value.ok_or_else(|| self.error(line, format!("no variable `{name}`")))
    }

    fn call(&mut self, name: &str, args: Vec<Value>, line: usize) -> Res<Value> {
        let Some(function) = self.functions.get(name).cloned() else {
            return builtin(name, &args).map_err(|err| self.error(line, err));
        };
        if args.len() != function.params.len() {
            let message = format!(
                "`{name}` takes {} value(s), got {}",
                function.params.len(),
                args.len()
            );
            return Err(self.error(line, message));
        }
        if self.locals.len() >= MAX_DEPTH {
            return Err(self.error(line, format!("`{name}` calls itself without end")));
        }
        self.locals
            .push(function.params.iter().cloned().zip(args).collect());
        let outer = std::mem::replace(&mut self.file, function.file.clone());
        let flow = self.block(&function.body);
        self.file = outer;
        self.locals.pop();
        Ok(match flow? {
            Flow::Return(value) => value,
            Flow::Exit(code) => {
                // Out of every caller: as an error that `run` turns back into the exit.
                return Err(Error {
                    file: EXIT.into(),
                    line: code as usize,
                    message: String::new(),
                    timeout: false,
                });
            }
            _ => Value::Nil,
        })
    }
}

/// Marks the error that carries an `exit` out of a function.
pub const EXIT: &str = "\u{0}exit";

fn compare(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left.number(), right.number()) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        _ => left.to_string().cmp(&right.to_string()),
    }
}

fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::List(a), Value::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Nil, other) | (other, Value::Nil) => *other == Value::Nil,
        _ => compare(left, right).is_eq(),
    }
}

fn binary(left: &Value, op: Op, right: &Value) -> Result<Value, String> {
    use std::cmp::Ordering::{Greater, Less};
    let numbers = || match (left.number(), right.number()) {
        (Some(a), Some(b)) => Ok((a, b)),
        (None, _) => Err(format!("`{left}` is no number")),
        (_, None) => Err(format!("`{right}` is no number")),
    };
    Ok(match op {
        Op::Add => match (left, right) {
            (Value::List(a), Value::List(b)) => Value::List(a.iter().chain(b).cloned().collect()),
            (Value::List(a), b) => Value::List(a.iter().chain([b]).cloned().collect()),
            (Value::Num(a), Value::Num(b)) => Value::Num(a + b),
            // Text that reads as a number adds as one only next to a number.
            (Value::Num(a), b) if b.number().is_some() => Value::Num(a + b.number().unwrap_or(0.0)),
            (a, Value::Num(b)) if a.number().is_some() => Value::Num(a.number().unwrap_or(0.0) + b),
            (a, b) => Value::Text(format!("{a}{b}")),
        },
        Op::Sub => numbers().map(|(a, b)| Value::Num(a - b))?,
        Op::Mul => numbers().map(|(a, b)| Value::Num(a * b))?,
        Op::Div | Op::Rem => {
            let (a, b) = numbers()?;
            if b == 0.0 {
                return Err("division by zero".into());
            }
            Value::Num(if op == Op::Div { a / b } else { a % b })
        }
        Op::Eq => Value::Bool(equal(left, right)),
        Op::Ne => Value::Bool(!equal(left, right)),
        Op::Lt => Value::Bool(compare(left, right) == Less),
        Op::Le => Value::Bool(compare(left, right) != Greater),
        Op::Gt => Value::Bool(compare(left, right) == Greater),
        Op::Ge => Value::Bool(compare(left, right) != Less),
        Op::Matches => Value::Bool(glob(&right.to_string(), &left.to_string())),
        Op::Contains => Value::Bool(holds(left, right)),
        Op::In => Value::Bool(holds(right, left)),
        Op::And | Op::Or => unreachable!("handled with their short cut"),
    })
}

fn holds(whole: &Value, part: &Value) -> bool {
    match whole {
        Value::List(items) => items.iter().any(|item| equal(item, part)),
        other => other.to_string().contains(&part.to_string()),
    }
}

/// The functions every macro has.
fn builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    let text = |at: usize| args.get(at).map(Value::to_string).unwrap_or_default();
    let number = |at: usize| {
        let value = args.get(at).and_then(Value::number);
        value.ok_or_else(|| format!("`{name}` needs a number"))
    };
    let count = |wanted: std::ops::RangeInclusive<usize>| match wanted.contains(&args.len()) {
        true => Ok(()),
        false => Err(format!(
            "`{name}` takes {wanted:?} value(s), got {}",
            args.len()
        )),
    };
    let texts =
        |items: Vec<&str>| Value::List(items.into_iter().map(|t| Value::Text(t.into())).collect());
    Ok(match name {
        "len" => {
            count(1..=1)?;
            Value::Num(match &args[0] {
                Value::List(items) => items.len() as f64,
                other => other.to_string().chars().count() as f64,
            })
        }
        "num" => {
            count(1..=1)?;
            Value::Num(
                args[0]
                    .number()
                    .ok_or_else(|| format!("`{}` is no number", args[0]))?,
            )
        }
        "str" => count(1..=1).map(|()| Value::Text(text(0)))?,
        "int" => count(1..=1)
            .and_then(|()| number(0))
            .map(|n| Value::Num(n.trunc()))?,
        "round" => count(1..=1)
            .and_then(|()| number(0))
            .map(|n| Value::Num(n.round()))?,
        "trim" => count(1..=1).map(|()| Value::Text(text(0).trim().to_string()))?,
        "upper" => count(1..=1).map(|()| Value::Text(text(0).to_uppercase()))?,
        "lower" => count(1..=1).map(|()| Value::Text(text(0).to_lowercase()))?,
        "lines" => count(1..=1).map(|()| texts(text(0).lines().collect()))?,
        "words" => count(1..=1).map(|()| texts(text(0).split_whitespace().collect()))?,
        "split" => {
            count(2..=2)?;
            let (whole, by) = (text(0), text(1));
            if by.is_empty() {
                return Err("`split` needs something to split by".into());
            }
            texts(whole.split(by.as_str()).collect())
        }
        "join" => {
            count(1..=2)?;
            let Value::List(items) = &args[0] else {
                return Err("`join` needs a list".into());
            };
            let items: Vec<String> = items.iter().map(Value::to_string).collect();
            Value::Text(items.join(&text(1)))
        }
        "replace" => count(3..=3).map(|()| Value::Text(text(0).replace(&text(1), &text(2))))?,
        "starts" => count(2..=2).map(|()| Value::Bool(text(0).starts_with(&text(1))))?,
        "ends" => count(2..=2).map(|()| Value::Bool(text(0).ends_with(&text(1))))?,
        "find" => {
            count(2..=2)?;
            let whole = text(0);
            let at = whole
                .find(&text(1))
                .map(|at| whole[..at].chars().count() as f64);
            Value::Num(at.unwrap_or(-1.0))
        }
        "slice" => {
            count(2..=3)?;
            let from = number(1)?.max(0.0) as usize;
            let take = match args.len() {
                3 => (number(2)?.max(0.0) as usize).saturating_sub(from),
                _ => usize::MAX,
            };
            match &args[0] {
                Value::List(items) => {
                    Value::List(items.iter().skip(from).take(take).cloned().collect())
                }
                other => Value::Text(other.to_string().chars().skip(from).take(take).collect()),
            }
        }
        "range" => {
            count(1..=2)?;
            let (from, to) = match args.len() {
                1 => (0, number(0)? as i64),
                _ => (number(0)? as i64, number(1)? as i64),
            };
            if to - from > 1_000_000 {
                return Err("`range` of more than a million".into());
            }
            Value::List((from..to).map(|n| Value::Num(n as f64)).collect())
        }
        "min" | "max" => {
            count(1..=usize::MAX)?;
            let items: &[Value] = match args {
                [Value::List(items)] => items,
                args => args,
            };
            let numbers: Option<Vec<f64>> = items.iter().map(Value::number).collect();
            let numbers = numbers
                .filter(|n| !n.is_empty())
                .ok_or(format!("`{name}` needs numbers"))?;
            let pick = if name == "min" { f64::min } else { f64::max };
            Value::Num(numbers.into_iter().reduce(pick).unwrap_or(0.0))
        }
        "env" => count(1..=1).map(|()| std::env::var(text(0)).map_or(Value::Nil, Value::Text))?,
        "exists" => count(1..=1).map(|()| Value::Bool(Path::new(&text(0)).exists()))?,
        "now" => {
            count(0..=0)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
            Value::Num(now.map_or(0.0, |now| now.as_secs_f64()))
        }
        _ => return Err(format!("no function `{name}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world that writes down what a macro does to it.
    #[derive(Default)]
    struct Fake {
        commands: Vec<String>,
        printed: Vec<String>,
        /// Looks until a wait for this window and condition word is met.
        waits: HashMap<(String, String), u32>,
        slept: Duration,
        files: HashMap<PathBuf, String>,
    }

    impl Host for Fake {
        fn command(&mut self, args: &[String]) -> Result<String, String> {
            self.commands.push(args.join(" "));
            match args[0].as_str() {
                "open" => Ok(args
                    .iter()
                    .skip_while(|a| *a != "--name")
                    .nth(1)
                    .cloned()
                    .unwrap_or("auto1".into())),
                "clip" => Ok("clipboard text".into()),
                "pixel" => Err("no invisible window".into()),
                _ => Ok(String::new()),
            }
        }

        fn wait(
            &mut self,
            name: &str,
            condition: &Condition,
            timeout: Duration,
            tick: &mut dyn FnMut(&mut dyn Host) -> Result<(), String>,
        ) -> Result<String, WaitError> {
            let word = match condition {
                Condition::Ready => "ready",
                Condition::Title(_) => "title",
                Condition::Closed => "closed",
                _ => "other",
            };
            let key = (name.to_string(), word.to_string());
            loop {
                match self.waits.get_mut(&key) {
                    Some(0) => return Ok(format!("{name} {word}")),
                    Some(left) if !timeout.is_zero() => *left -= 1,
                    _ => return Err(WaitError::Timeout("nothing".into())),
                }
                // Each look of the fake takes as long as the handlers rest.
                std::thread::sleep(HANDLER_PERIOD);
                tick(self).map_err(WaitError::Failed)?;
            }
        }

        fn print(&mut self, line: &str) {
            self.printed.push(line.to_string());
        }

        fn ask(&mut self, prompt: &str) -> Result<String, String> {
            Ok(format!("answer to {prompt}"))
        }

        fn shell(&mut self, command: &str) -> Result<String, String> {
            match command {
                "false" => Err("`false` ended with 1: ".into()),
                command => Ok(format!("ran {command}")),
            }
        }

        fn sleep(&mut self, time: Duration) {
            self.slept += time;
        }

        fn read(&mut self, path: &Path) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or(format!("{}: not there", path.display()))
        }
    }

    const COMMANDS: &[&str] = &["open", "close", "type", "key", "clip", "pixel"];

    fn run_in(fake: &mut Fake, source: &str, given: &[(&str, &str)]) -> Res<Outcome> {
        let given = given
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Interp::new(fake, COMMANDS, given).run(source, Path::new("test.wm"))
    }

    /// What a macro prints.
    fn printed(source: &str) -> Vec<String> {
        let mut fake = Fake::default();
        run_in(&mut fake, source, &[]).unwrap();
        fake.printed
    }

    fn failure(source: &str) -> Error {
        run_in(&mut Fake::default(), source, &[]).unwrap_err()
    }

    #[test]
    fn values_and_expressions() {
        let out = printed(
            "let a = 2 + 3 * 4
             let b = \"x\" + 1
             let list = [1, two, \"3\"]
             print $a $b $list len($list) $list[1] $list[-1] $list[9]
             print (10 / 4) (7 % 3) (-$a) (\"5\" + 1) (\"5\" == 5) (\"abc\" < \"abd\")
             print (1 == 1 and not false) (nil or \"fallback\") (\"Example Domain\" matches \"Ex*\")
             print ($list contains two) (3 in $list) (\"hay\" contains \"a\")
             print upper(\"a\") trim(\"  b \") replace(\"aXa\", \"X\", \"-\") join(split(\"a,b\", \",\"), \"+\")
             print slice(\"hello\", 1, 3) find(\"hello\", \"l\") range(2, 5) max(1, 7, 3) min([4, 2])
             print \"price: $$5 for $a\" 'raw $a' pre${a}post",
        );
        assert_eq!(
            out,
            [
                "14 x1 [1, two, 3] 3 two 3 ",
                "2.5 1 -14 6 true true",
                "true fallback true",
                "true true true",
                "A b a-a a+b",
                "el 2 [2, 3, 4] 7 2",
                "price: $5 for 14 raw $a pre14post",
            ]
        );
    }

    #[test]
    fn control_flow() {
        let out = printed(
            "let n = 0
             while true
                 set n = $n + 1
                 if $n == 2
                     continue
                 elif $n > 4
                     break
                 else
                     print n $n
                 end
             end
             for x in [a, b]
                 print item $x
             end
             repeat 2
                 print again
             end
             func fact(n)
                 if $n <= 1
                     return 1
                 end
                 return ($n * fact($n - 1))
             end
             func shout(text)
                 print upper($text)
             end
             shout(hi)
             print fact(5) $n",
        );
        assert_eq!(
            out,
            [
                "n 1", "n 3", "n 4", "item a", "item b", "again", "again", "HI", "120 5"
            ]
        );
    }

    #[test]
    fn steps_reach_the_host() {
        let mut fake = Fake::default();
        fake.waits.insert(("b1".into(), "ready".into()), 0);
        let outcome = run_in(
            &mut fake,
            "param url = \"https://example.com\"
             param tries
             open b1 = --size 800x600 firefox
             let other = open kitty
             wait b1 ready
             type b1 \"$url\"
             key b1 Return CTRL+l
             let text = clip b1 get
             let out = shell echo hi
             let name = ask Your name?
             sleep 500ms
             print $other $text / $out / $name / $tries
             close b1
             return $text",
            &[("tries", "3")],
        );
        assert_eq!(
            outcome,
            Ok(Outcome::Done(Value::Text("clipboard text".into())))
        );
        assert_eq!(
            fake.commands,
            [
                "open --name b1 --size 800x600 firefox",
                "open kitty",
                "type b1 https://example.com",
                "key b1 Return CTRL+l",
                "clip b1 get",
                "close b1",
            ]
        );
        assert_eq!(
            fake.printed,
            ["auto1 clipboard text / ran echo hi / answer to Your name? / 3"]
        );
        assert_eq!(fake.slept, Duration::from_millis(500));
    }

    #[test]
    fn failures_are_caught_or_named_with_their_line() {
        let out = printed(
            "try
                 pixel b1 1 1
                 print not reached
             else
                 print caught: $error
             end
             try
                 fail \"my\" reason
             else
                 print $error
             end
             try
                 shell false
             end
             print on",
        );
        assert_eq!(out, ["caught: no invisible window", "my reason", "on"]);

        let err = failure("print a\n\nfail broke it");
        assert_eq!(
            (err.line, err.message.as_str(), err.timeout),
            (3, "broke it", false)
        );
        assert_eq!(err.to_string(), "test.wm:3: broke it");
        assert_eq!(failure("print $nothing").message, "no variable `nothing`");
        assert_eq!(failure("set x = 1").line, 1);
        assert_eq!(failure("print (1 / 0)").message, "division by zero");
        assert_eq!(failure("print (\"a\" - 1)").message, "`a` is no number");
        assert_eq!(failure("print nope(1)").message, "no function `nope`");
        assert_eq!(failure("func f(a)\nend\nf()").line, 3);
        assert_eq!(
            failure("func f()\nf()\nend\nf()").message,
            "`f` calls itself without end"
        );
        assert_eq!(
            failure("param needed").message,
            "the macro needs `needed=VALUE`"
        );
        assert_eq!(failure("print a\nif true").line, 2);
        let err = run_in(&mut Fake::default(), "print a", &[("typo", "1")]).unwrap_err();
        assert_eq!(err.message, "the macro has no parameter `typo`");

        let err = failure("timeout 1s\nwait b1 ready");
        assert!(err.timeout && err.line == 2, "{err:?}");
        let out = printed("try\nwait b1 ready timeout 10ms\nelse\nprint $error\nend");
        assert_eq!(out, ["timeout: nothing"]);
        assert_eq!(failure("wait b1 dance").line, 1);
    }

    #[test]
    fn exit_ends_everything_even_from_a_function() {
        let mut fake = Fake::default();
        let source =
            "func leave()\ntry\nexit 4\nelse\nprint caught\nend\nend\nleave()\nprint after";
        assert_eq!(run_in(&mut fake, source, &[]), Ok(Outcome::Exit(4)));
        assert_eq!(fake.printed, Vec::<String>::new());
        assert_eq!(run_in(&mut fake, "exit", &[]), Ok(Outcome::Exit(0)));
        assert_eq!(
            run_in(&mut fake, "return [1, \"a\"]", &[]).map(|outcome| match outcome {
                Outcome::Done(value) => value.to_json(),
                Outcome::Exit(_) => String::new(),
            }),
            Ok("[1,\"a\"]".into())
        );
    }

    #[test]
    fn a_handler_runs_when_its_condition_appears_during_a_wait() {
        let mut fake = Fake::default();
        // The window is ready after three looks; a title shows from the start.
        fake.waits.insert(("b1".into(), "ready".into()), 3);
        fake.waits.insert(("b1".into(), "title".into()), 0);
        let source = "on b1 title \"*Cookies*\" do
                          key b1 Escape
                          print dismissed
                      end
                      let seen = wait b1 ready
                      print $seen
                      off b1
                      wait b1 ready";
        assert_eq!(
            run_in(&mut fake, source, &[]),
            Ok(Outcome::Done(Value::Nil))
        );
        // Once, not on every look: it fires when the condition appears.
        assert_eq!(fake.commands, ["key b1 Escape"]);
        assert_eq!(fake.printed, ["dismissed", "b1 ready"]);
        assert_eq!(failure("on b1 clipboard do\nend").line, 1);
    }

    #[test]
    fn use_brings_the_functions_of_another_file() {
        let mut fake = Fake::default();
        let dir = std::env::temp_dir().join(format!("wirbel-macro-use-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.wm");
        std::fs::write(&lib, "").unwrap();
        fake.files.insert(
            lib.clone(),
            "func twice(x)\nreturn ($x * 2)\nend\nlet shared = 7".into(),
        );
        let main = dir.join("main.wm");
        let given = HashMap::new();
        let outcome = Interp::new(&mut fake, COMMANDS, given)
            .run("use lib\nuse \"lib.wm\"\nprint twice($shared)", &main);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(outcome, Ok(Outcome::Done(Value::Nil)));
        assert_eq!(fake.printed, ["14"]);
        assert_eq!(failure("use nothing").message, "no macro file `nothing`");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("20s"), Ok(Duration::from_secs(20)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("1.5"), Ok(Duration::from_millis(1500)));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("-1s").is_err());
    }
}
