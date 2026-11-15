//! Reading a macro: lines into steps, text into expressions. `docs/vwm/MACROS.md` describes the
//! language.

use std::rc::Rc;

/// A piece of a text with variables in it.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Lit(String),
    Var(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Nil,
    Bool(bool),
    Num(f64),
    /// A text, with the variables it names put in.
    Text(Vec<Piece>),
    Var(String),
    List(Vec<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Binary(Box<Expr>, Op, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Matches,
    Contains,
    In,
}

/// What a step yields a value from: an expression, or another step such as `shell` or `wait`.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Expr(Expr),
    Step(Box<Step>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub line: usize,
    pub kind: Kind,
}

pub type Block = Rc<Vec<Step>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Let(String, Source),
    Set(String, Source),
    Param(String, Option<Source>),
    Print(Vec<Expr>),
    If(Vec<(Expr, Block)>, Option<Block>),
    While(Expr, Block),
    For(String, Expr, Block),
    Repeat(Expr, Block),
    Func(String, Vec<String>, Block),
    Return(Vec<Expr>),
    Try(Block, Option<Block>),
    Fail(Vec<Expr>),
    Exit(Option<Expr>),
    Sleep(Expr),
    Timeout(Expr),
    Use(Expr),
    /// `on TARGET CONDITION... do`: the words and the steps to run.
    On(Vec<Expr>, Block),
    Off(Vec<Expr>),
    Break,
    Continue,
    Wait(Vec<Expr>),
    Shell(Vec<Expr>),
    Ask(Vec<Expr>),
    Read(Vec<Expr>),
    /// `write` or `append`.
    Write(bool, Vec<Expr>),
    /// A command for invisible windows, as `wirbel vwm` takes it.
    Command(Vec<Expr>),
    Call(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub line: usize,
    pub message: String,
}

fn fail<T>(line: usize, message: impl Into<String>) -> Result<T, SyntaxError> {
    Err(SyntaxError {
        line,
        message: message.into(),
    })
}

/// The steps that yield a value and are no commands for invisible windows.
const VALUE_STEPS: [&str; 4] = ["wait", "shell", "ask", "read"];

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// A line without its comment: a `#` at the start or after a space, outside of quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut before = ' ';
    let mut escaped = false;
    for (at, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && before.is_whitespace() => return &line[..at],
            None => {}
        }
        before = c;
    }
    line
}

/// The pieces of a text in which `$name` and `${name}` stand for variables and `$$` for a dollar.
fn pieces(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] != '$' {
            lit.push(chars[at]);
            at += 1;
            continue;
        }
        let rest = &chars[at + 1..];
        let (name, used): (String, usize) = match rest.first() {
            Some('$') => {
                lit.push('$');
                at += 2;
                continue;
            }
            Some('{') => match rest.iter().position(|c| *c == '}') {
                Some(end) => (rest[1..end].iter().collect(), end + 1),
                None => (String::new(), 0),
            },
            _ => {
                let name: String = rest
                    .iter()
                    .take_while(|c| c.is_alphanumeric() || **c == '_')
                    .collect();
                let used = name.chars().count();
                (name, used)
            }
        };
        if is_name(&name) {
            if !lit.is_empty() {
                pieces.push(Piece::Lit(std::mem::take(&mut lit)));
            }
            pieces.push(Piece::Var(name));
            at += 1 + used;
        } else {
            lit.push('$');
            at += 1;
        }
    }
    if !lit.is_empty() || pieces.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    pieces
}

/// A text as an expression: a lone variable keeps its kind, anything else becomes text.
fn text_expr(text: &str) -> Expr {
    let mut pieces = pieces(text);
    match pieces.as_slice() {
        [Piece::Var(_)] => match pieces.remove(0) {
            Piece::Var(name) => Expr::Var(name),
            Piece::Lit(_) => unreachable!(),
        },
        _ => Expr::Text(pieces),
    }
}

/// Reads a `"double quoted"` text from after its opening quote. Returns the text with the
/// escapes resolved (a `\$` as `$$`, to stay a dollar) and how many characters it took.
fn quoted(chars: &[char], line: usize) -> Result<(String, usize), SyntaxError> {
    let mut text = String::new();
    let mut at = 0;
    loop {
        match chars.get(at) {
            None => return fail(line, "a text in quotes does not end"),
            Some('"') => return Ok((text, at + 1)),
            Some('\\') => {
                match chars.get(at + 1) {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('$') => text.push_str("$$"),
                    Some(c @ ('"' | '\\')) => text.push(*c),
                    _ => return fail(line, "unknown escape, use \\n \\t \\\" \\\\ or \\$"),
                }
                at += 2;
            }
            Some(c) => {
                text.push(*c);
                at += 1;
            }
        }
    }
}

/// The words of a step: separated by spaces; `"..."` is one word with variables put in,
/// `'...'` one word as it stands, `(...)` an expression, anything else a word in which
/// `$name` is put in.
pub fn words(text: &str, line: usize) -> Result<Vec<Expr>, SyntaxError> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        match chars[at] {
            c if c.is_whitespace() => at += 1,
            '"' => {
                let (text, used) = quoted(&chars[at + 1..], line)?;
                words.push(Expr::Text(pieces(&text)));
                at += 1 + used;
            }
            '\'' => {
                let end = chars[at + 1..].iter().position(|c| *c == '\'');
                let Some(end) = end else {
                    return fail(line, "a text in quotes does not end");
                };
                let text: String = chars[at + 1..at + 1 + end].iter().collect();
                words.push(Expr::Text(vec![Piece::Lit(text)]));
                at += end + 2;
            }
            '(' => {
                let end = closing(&chars[at..], line)?;
                let inner: String = chars[at + 1..at + end].iter().collect();
                words.push(expression(&inner, line)?);
                at += end + 1;
            }
            _ => {
                // `name(...)` and `$name[...]` are expressions, spaces inside them included.
                if let Some(end) = call_end(&chars[at..]) {
                    let text: String = chars[at..at + end].iter().collect();
                    if let Ok(expr) = expression(&text, line) {
                        words.push(expr);
                        at += end;
                        continue;
                    }
                }
                let end = chars[at..].iter().position(|c| c.is_whitespace());
                let end = end.map_or(chars.len(), |end| at + end);
                let word: String = chars[at..end].iter().collect();
                words.push(text_expr(&word));
                at = end;
            }
        }
    }
    Ok(words)
}

/// If `chars` begin with a call `name(...)` or with `$name[...]`, where that ends: after the
/// last bracket, which has to be the end of the word.
fn call_end(chars: &[char]) -> Option<usize> {
    let dollar = usize::from(chars.first() == Some(&'$'));
    let name = chars[dollar..]
        .iter()
        .take_while(|c| c.is_alphanumeric() || **c == '_')
        .count();
    let open = *chars.get(dollar + name)?;
    let fits = match dollar {
        0 => open == '(',
        _ => open == '[',
    };
    if name == 0 || !fits || chars[dollar].is_ascii_digit() {
        return None;
    }
    let (mut depth, mut quote) = (0, None);
    for (at, c) in chars.iter().enumerate().skip(dollar + name) {
        match quote {
            Some(q) if *c == q => quote = None,
            Some(_) => {}
            None if *c == '"' || *c == '\'' => quote = Some(*c),
            None if *c == '(' || *c == '[' => depth += 1,
            None if *c == ')' || *c == ']' => {
                depth -= 1;
                let next = chars.get(at + 1);
                if depth == 0 && next != Some(&'[') {
                    return next.is_none_or(|c| c.is_whitespace()).then_some(at + 1);
                }
            }
            None => {}
        }
    }
    None
}

/// Where the bracket that opens at the start of `chars` closes.
fn closing(chars: &[char], line: usize) -> Result<usize, SyntaxError> {
    let (mut depth, mut quote, mut escaped) = (0, None, false);
    for (at, c) in chars.iter().enumerate() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if *c == '\\' => escaped = true,
            Some(q) if *c == q => quote = None,
            Some(_) => {}
            None if *c == '"' || *c == '\'' => quote = Some(*c),
            None if *c == '(' => depth += 1,
            None if *c == ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(at);
                }
            }
            None => {}
        }
    }
    fail(line, "a bracket does not close")
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Text(Vec<Piece>),
    Var(String),
    Word(String),
    Sym(&'static str),
}

const SYMBOLS: [&str; 18] = [
    "==", "!=", "<=", ">=", "<", ">", "+", "-", "*", "/", "%", "(", ")", "[", "]", ",", "=", "!",
];

fn tokens(text: &str, line: usize) -> Result<Vec<Token>, SyntaxError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    'next: while at < chars.len() {
        let c = chars[at];
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        if c == '"' {
            let (text, used) = quoted(&chars[at + 1..], line)?;
            tokens.push(Token::Text(pieces(&text)));
            at += 1 + used;
            continue;
        }
        if c == '\'' {
            let Some(end) = chars[at + 1..].iter().position(|c| *c == '\'') else {
                return fail(line, "a text in quotes does not end");
            };
            let text: String = chars[at + 1..at + 1 + end].iter().collect();
            tokens.push(Token::Text(vec![Piece::Lit(text)]));
            at += end + 2;
            continue;
        }
        if c == '$' {
            let name: String = chars[at + 1..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '_')
                .collect();
            if !is_name(&name) {
                return fail(line, "a name has to follow the $");
            }
            at += 1 + name.chars().count();
            tokens.push(Token::Var(name));
            continue;
        }
        if c.is_ascii_digit() {
            let number: String = chars[at..]
                .iter()
                .take_while(|c| c.is_ascii_digit() || **c == '.')
                .collect();
            let Ok(value) = number.parse() else {
                return fail(line, format!("`{number}` is no number"));
            };
            at += number.len();
            tokens.push(Token::Num(value));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let word: String = chars[at..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '_')
                .collect();
            at += word.chars().count();
            tokens.push(Token::Word(word));
            continue;
        }
        for symbol in SYMBOLS {
            let fits = symbol
                .chars()
                .enumerate()
                .all(|(i, s)| chars.get(at + i) == Some(&s));
            if fits {
                tokens.push(Token::Sym(symbol));
                at += symbol.len();
                continue 'next;
            }
        }
        return fail(
            line,
            format!("unexpected `{c}` in an expression, text goes in quotes"),
        );
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    line: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn sym(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Sym(s)) if *s == symbol);
        self.at += usize::from(found);
        found
    }

    fn word(&mut self, word: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Word(w)) if w == word);
        self.at += usize::from(found);
        found
    }

    fn binary(
        &mut self,
        next: fn(&mut Self) -> Result<Expr, SyntaxError>,
        ops: &[(&str, Op)],
        once: bool,
    ) -> Result<Expr, SyntaxError> {
        let mut left = next(self)?;
        'more: loop {
            for (name, op) in ops {
                if self.sym(name) || self.word(name) {
                    let right = next(self)?;
                    left = Expr::Binary(Box::new(left), *op, Box::new(right));
                    if once {
                        return Ok(left);
                    }
                    continue 'more;
                }
            }
            return Ok(left);
        }
    }

    fn or(&mut self) -> Result<Expr, SyntaxError> {
        self.binary(Self::and, &[("or", Op::Or)], false)
    }

    fn and(&mut self) -> Result<Expr, SyntaxError> {
        self.binary(Self::not, &[("and", Op::And)], false)
    }

    fn not(&mut self) -> Result<Expr, SyntaxError> {
        if self.word("not") || self.sym("!") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Expr, SyntaxError> {
        let ops = [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
            ("matches", Op::Matches),
            ("contains", Op::Contains),
            ("in", Op::In),
        ];
        self.binary(Self::sum, &ops, true)
    }

    fn sum(&mut self) -> Result<Expr, SyntaxError> {
        self.binary(Self::product, &[("+", Op::Add), ("-", Op::Sub)], false)
    }

    fn product(&mut self) -> Result<Expr, SyntaxError> {
        let ops = [("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)];
        self.binary(Self::unary, &ops, false)
    }

    fn unary(&mut self) -> Result<Expr, SyntaxError> {
        if self.sym("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        let mut value = self.primary()?;
        while self.sym("[") {
            let index = self.or()?;
            self.expect("]")?;
            value = Expr::Index(Box::new(value), Box::new(index));
        }
        Ok(value)
    }

    fn expect(&mut self, symbol: &str) -> Result<(), SyntaxError> {
        match self.sym(symbol) {
            true => Ok(()),
            false => fail(self.line, format!("expected `{symbol}`")),
        }
    }

    fn list(&mut self, end: &str) -> Result<Vec<Expr>, SyntaxError> {
        let mut items = Vec::new();
        if self.sym(end) {
            return Ok(items);
        }
        loop {
            items.push(self.or()?);
            if !self.sym(",") {
                self.expect(end)?;
                return Ok(items);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        let token = self.peek().cloned();
        self.at += 1;
        Ok(match token {
            Some(Token::Num(value)) => Expr::Num(value),
            Some(Token::Text(pieces)) => Expr::Text(pieces),
            Some(Token::Var(name)) => Expr::Var(name),
            Some(Token::Sym("(")) => {
                let inner = self.or()?;
                self.expect(")")?;
                inner
            }
            Some(Token::Sym("[")) => Expr::List(self.list("]")?),
            Some(Token::Word(word)) => match word.as_str() {
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "nil" => Expr::Nil,
                _ if self.sym("(") => Expr::Call(word, self.list(")")?),
                // A plain word is itself, as in the words of a step.
                _ => Expr::Text(vec![Piece::Lit(word)]),
            },
            Some(Token::Sym(symbol)) => {
                return fail(self.line, format!("unexpected `{symbol}` in an expression"));
            }
            None => return fail(self.line, "an expression ends too early"),
        })
    }
}

/// Reads an expression.
pub fn expression(text: &str, line: usize) -> Result<Expr, SyntaxError> {
    let mut parser = Parser {
        tokens: tokens(text, line)?,
        at: 0,
        line,
    };
    let expr = parser.or()?;
    if parser.at < parser.tokens.len() {
        return fail(line, "unexpected text after the expression");
    }
    Ok(expr)
}

struct Lines<'a> {
    /// Line number, first word, the rest.
    lines: Vec<(usize, &'a str, &'a str)>,
    at: usize,
    /// The commands for invisible windows.
    commands: &'a [&'a str],
}

/// Reads a macro. `commands` are the words that are commands for invisible windows.
pub fn parse(source: &str, commands: &[&str]) -> Result<Vec<Step>, SyntaxError> {
    let lines = source.lines().enumerate().filter_map(|(index, line)| {
        let line = strip_comment(line).trim();
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        (!line.is_empty()).then_some((index + 1, word, rest.trim()))
    });
    let mut lines = Lines {
        lines: lines.collect(),
        at: 0,
        commands,
    };
    let (steps, end) = lines.block(&[])?;
    match end {
        None => Ok(steps),
        Some((line, word)) => fail(line, format!("`{word}` without something to end")),
    }
}

impl<'a> Lines<'a> {
    /// Steps up to one of the words in `until` (returned with its line) or the end of the file.
    #[allow(clippy::type_complexity)]
    fn block(
        &mut self,
        until: &[&str],
    ) -> Result<(Vec<Step>, Option<(usize, &'a str)>), SyntaxError> {
        let mut steps = Vec::new();
        while let Some((line, word, rest)) = self.lines.get(self.at).copied() {
            self.at += 1;
            if until.contains(&word) || ["end", "else", "elif"].contains(&word) {
                if !until.contains(&word) {
                    return fail(line, format!("`{word}` does not belong here"));
                }
                // `elif` needs its condition again.
                if word == "elif" {
                    self.at -= 1;
                }
                return Ok((steps, Some((line, word))));
            }
            let kind = self.step(line, word, rest)?;
            steps.push(Step { line, kind });
        }
        Ok((steps, None))
    }

    /// The steps of a block that `end` closes.
    fn body(&mut self, opened: usize, what: &str) -> Result<Block, SyntaxError> {
        match self.block(&["end"])? {
            (steps, Some(_)) => Ok(Rc::new(steps)),
            (_, None) => fail(opened, format!("this `{what}` has no `end`")),
        }
    }

    fn source(&mut self, line: usize, text: &str) -> Result<Source, SyntaxError> {
        let word = text.split_whitespace().next().unwrap_or_default();
        if VALUE_STEPS.contains(&word) || self.commands.contains(&word) {
            let rest = text[word.len()..].trim();
            let kind = self.step(line, word, rest)?;
            return Ok(Source::Step(Box::new(Step { line, kind })));
        }
        Ok(Source::Expr(expression(text, line)?))
    }

    /// `NAME = SOURCE`.
    fn assignment(
        &mut self,
        line: usize,
        rest: &str,
        what: &str,
    ) -> Result<(String, Source), SyntaxError> {
        let Some((name, value)) = rest.split_once('=') else {
            return fail(line, format!("expected `{what} NAME = VALUE`"));
        };
        let name = name.trim().trim_start_matches('$');
        if !is_name(name) {
            return fail(line, format!("`{name}` cannot be a name"));
        }
        Ok((name.to_string(), self.source(line, value.trim())?))
    }

    fn step(&mut self, line: usize, word: &str, rest: &str) -> Result<Kind, SyntaxError> {
        let none = |kind: Kind| match rest {
            "" => Ok(kind),
            _ => fail(line, format!("`{word}` takes nothing after it")),
        };
        Ok(match word {
            "let" => {
                let (name, source) = self.assignment(line, rest, "let")?;
                Kind::Let(name, source)
            }
            "set" => {
                let (name, source) = self.assignment(line, rest, "set")?;
                Kind::Set(name, source)
            }
            "param" if !rest.contains('=') => {
                if !is_name(rest) {
                    return fail(line, "expected `param NAME` or `param NAME = DEFAULT`");
                }
                Kind::Param(rest.to_string(), None)
            }
            "param" => {
                let (name, source) = self.assignment(line, rest, "param")?;
                Kind::Param(name, Some(source))
            }
            "print" => Kind::Print(words(rest, line)?),
            "fail" => Kind::Fail(words(rest, line)?),
            // A whole expression if it is one, such as a list; else words.
            "return" => match expression(rest, line) {
                Ok(expr) if !rest.is_empty() => Kind::Return(vec![expr]),
                _ => Kind::Return(words(rest, line)?),
            },
            "exit" if rest.is_empty() => Kind::Exit(None),
            "exit" => Kind::Exit(Some(expression(rest, line)?)),
            "sleep" => Kind::Sleep(self.one(line, word, rest)?),
            "timeout" => Kind::Timeout(self.one(line, word, rest)?),
            "use" => Kind::Use(self.one(line, word, rest)?),
            "break" => none(Kind::Break)?,
            "continue" => none(Kind::Continue)?,
            "if" => {
                let mut branches = Vec::new();
                let mut otherwise = None;
                let mut condition = expression(rest, line)?;
                loop {
                    let (steps, end) = self.block(&["end", "else", "elif"])?;
                    branches.push((condition.clone(), Rc::new(steps)));
                    match end {
                        Some((_, "end")) => break,
                        Some((_, "elif")) => {
                            let (at, _, rest) = self.lines[self.at];
                            self.at += 1;
                            condition = expression(rest, at)?;
                        }
                        Some((at, _)) => {
                            otherwise = Some(self.body(at, "else")?);
                            break;
                        }
                        None => return fail(line, "this `if` has no `end`"),
                    }
                }
                Kind::If(branches, otherwise)
            }
            "while" => Kind::While(expression(rest, line)?, self.body(line, word)?),
            "repeat" => Kind::Repeat(expression(rest, line)?, self.body(line, word)?),
            "for" => {
                let parts = rest.split_once(" in ");
                let Some((name, list)) = parts.map(|(name, list)| (name.trim(), list)) else {
                    return fail(line, "expected `for NAME in LIST`");
                };
                let name = name.trim_start_matches('$');
                if !is_name(name) {
                    return fail(line, format!("`{name}` cannot be a name"));
                }
                Kind::For(
                    name.to_string(),
                    expression(list, line)?,
                    self.body(line, word)?,
                )
            }
            "func" => {
                let open = rest.find('(');
                let parts = open.and_then(|open| {
                    let inner = rest[open + 1..].trim_end().strip_suffix(')')?;
                    Some((rest[..open].trim(), inner))
                });
                let Some((name, params)) = parts.filter(|(name, _)| is_name(name)) else {
                    return fail(line, "expected `func NAME(A, B)`");
                };
                let params = params
                    .split(',')
                    .map(|param| param.trim().trim_start_matches('$'));
                let params: Vec<String> = params
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
                if let Some(bad) = params.iter().find(|param| !is_name(param)) {
                    return fail(line, format!("`{bad}` cannot be a name"));
                }
                Kind::Func(name.to_string(), params, self.body(line, word)?)
            }
            "try" => {
                none(Kind::Break)?;
                match self.block(&["end", "else"])? {
                    (steps, Some((_, "end"))) => Kind::Try(Rc::new(steps), None),
                    (steps, Some((at, _))) => {
                        Kind::Try(Rc::new(steps), Some(self.body(at, "else")?))
                    }
                    (_, None) => return fail(line, "this `try` has no `end`"),
                }
            }
            "on" => {
                let Some(head) = rest.strip_suffix("do").filter(|head| head.ends_with(' ')) else {
                    return fail(line, "expected `on NAME CONDITION do`");
                };
                let head = words(head, line)?;
                if head.len() < 2 {
                    return fail(line, "expected `on NAME CONDITION do`");
                }
                Kind::On(head, self.body(line, word)?)
            }
            "off" => Kind::Off(words(rest, line)?),
            "wait" => Kind::Wait(self.some(line, word, rest)?),
            "shell" => Kind::Shell(self.some(line, word, rest)?),
            "ask" => Kind::Ask(words(rest, line)?),
            "read" => Kind::Read(self.some(line, word, rest)?),
            "write" => Kind::Write(false, self.some(line, word, rest)?),
            "append" => Kind::Write(true, self.some(line, word, rest)?),
            _ if self.commands.contains(&word) => {
                let mut all = vec![Expr::Text(vec![Piece::Lit(word.to_string())])];
                all.extend(words(rest, line)?);
                Kind::Command(all)
            }
            _ => {
                // `name(arguments)` on a line of its own calls a function.
                let whole = self.lines[self.at - 1];
                let text = format!("{} {}", whole.1, whole.2);
                match expression(&text, line) {
                    Ok(call @ Expr::Call(..)) => Kind::Call(call),
                    _ => return fail(line, format!("unknown step `{word}`")),
                }
            }
        })
    }

    fn some(&self, line: usize, word: &str, rest: &str) -> Result<Vec<Expr>, SyntaxError> {
        let words = words(rest, line)?;
        match words.is_empty() {
            true => fail(line, format!("`{word}` needs something after it")),
            false => Ok(words),
        }
    }

    fn one(&self, line: usize, word: &str, rest: &str) -> Result<Expr, SyntaxError> {
        let mut words = words(rest, line)?;
        match words.len() {
            1 => Ok(words.remove(0)),
            _ => fail(line, format!("`{word}` takes one value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: [&str; 4] = ["open", "type", "key", "close"];

    fn lit(text: &str) -> Expr {
        Expr::Text(vec![Piece::Lit(text.into())])
    }

    fn kinds(source: &str) -> Vec<Kind> {
        let steps = parse(source, &COMMANDS).unwrap();
        steps.into_iter().map(|step| step.kind).collect()
    }

    fn error(source: &str) -> SyntaxError {
        parse(source, &COMMANDS).unwrap_err()
    }

    #[test]
    fn words_keep_quotes_variables_and_expressions_apart() {
        let words = words(
            r#"b1 "a $x b" 'c $y' $z pre$z-x ($n + 1) CTRL+l --size=1x1"#,
            1,
        )
        .unwrap();
        assert_eq!(words[0], lit("b1"));
        assert_eq!(
            words[1],
            Expr::Text(vec![
                Piece::Lit("a ".into()),
                Piece::Var("x".into()),
                Piece::Lit(" b".into())
            ])
        );
        assert_eq!(words[2], lit("c $y"));
        assert_eq!(words[3], Expr::Var("z".into()));
        assert_eq!(
            words[4],
            Expr::Text(vec![
                Piece::Lit("pre".into()),
                Piece::Var("z".into()),
                Piece::Lit("-x".into())
            ])
        );
        assert_eq!(
            words[5],
            Expr::Binary(
                Box::new(Expr::Var("n".into())),
                Op::Add,
                Box::new(Expr::Num(1.0))
            )
        );
        assert_eq!(words[6], lit("CTRL+l"));
        assert_eq!(words[7], lit("--size=1x1"));
        assert_eq!(
            super::words(r#""a\"b\n\$c $$d ${e}f""#, 1).unwrap(),
            [Expr::Text(vec![
                Piece::Lit("a\"b\n$c $d ".into()),
                Piece::Var("e".into()),
                Piece::Lit("f".into())
            ])]
        );
        assert!(super::words("\"open", 3).is_err());
        assert!(super::words("(1 + 2", 3).is_err());
    }

    #[test]
    fn expressions_bind_as_expected() {
        let expr = expression("1 + 2 * 3 == 7 and not $done or len($list) > 0", 1).unwrap();
        let Expr::Binary(left, Op::Or, right) = expr else {
            panic!("or is not the loosest");
        };
        assert!(matches!(*left, Expr::Binary(_, Op::And, _)));
        assert!(matches!(*right, Expr::Binary(_, Op::Gt, _)));
        assert_eq!(
            expression("$a[0][1]", 1).unwrap(),
            Expr::Index(
                Box::new(Expr::Index(
                    Box::new(Expr::Var("a".into())),
                    Box::new(Expr::Num(0.0))
                )),
                Box::new(Expr::Num(1.0))
            )
        );
        assert_eq!(
            expression("[1, x, \"y\"]", 1).unwrap(),
            Expr::List(vec![Expr::Num(1.0), lit("x"), lit("y")])
        );
        assert_eq!(
            expression("-(2)", 1).unwrap(),
            Expr::Neg(Box::new(Expr::Num(2.0)))
        );
        assert!(matches!(
            expression("$t matches \"*x*\"", 1).unwrap(),
            Expr::Binary(_, Op::Matches, _)
        ));
        for bad in ["1 +", "(1", "1 2", "a @ b", "$", "[1,", "f(1"] {
            assert!(expression(bad, 4).is_err(), "{bad}");
        }
    }

    #[test]
    fn steps_and_blocks() {
        let steps = kinds(
            "# a comment
             param url = \"https://example.com\"   # the page
             open b1 = firefox
             type b1 \"$url # not a comment\"
             let n = 1 + 1
             let out = shell echo hi
             if $n == 2
                 print two
             elif $n == 3
                 print three
             else
                 print other
             end
             while $n < 5
                 set n = $n + 1
                 break
             end
             for x in [1, 2]
                 continue
             end
             try
                 fail \"no\"
             else
                 print $error
             end
             func add(a, b)
                 return ($a + $b)
             end
             add(1, 2)
             on b1 title \"*Cookies*\" do
                 key b1 Escape
             end
             close b1",
        );
        assert!(matches!(&steps[0], Kind::Param(name, Some(Source::Expr(_))) if name == "url"));
        assert!(
            matches!(&steps[1], Kind::Command(words) if words.len() == 4 && words[2] == lit("="))
        );
        let Kind::Command(words) = &steps[2] else {
            panic!()
        };
        assert_eq!(
            words[2],
            Expr::Text(vec![
                Piece::Var("url".into()),
                Piece::Lit(" # not a comment".into())
            ])
        );
        assert!(matches!(
            &steps[3],
            Kind::Let(_, Source::Expr(Expr::Binary(..)))
        ));
        assert!(
            matches!(&steps[4], Kind::Let(_, Source::Step(step)) if matches!(step.kind, Kind::Shell(_)))
        );
        assert!(matches!(&steps[5], Kind::If(branches, Some(_)) if branches.len() == 2));
        assert!(matches!(&steps[6], Kind::While(_, body) if body.len() == 2));
        assert!(matches!(&steps[7], Kind::For(name, _, _) if name == "x"));
        assert!(matches!(&steps[8], Kind::Try(_, Some(_))));
        assert!(
            matches!(&steps[9], Kind::Func(name, params, _) if name == "add" && params.len() == 2)
        );
        assert!(
            matches!(&steps[10], Kind::Call(Expr::Call(name, args)) if name == "add" && args.len() == 2)
        );
        assert!(matches!(&steps[11], Kind::On(head, body) if head.len() == 3 && body.len() == 1));
        assert_eq!(steps.len(), 13);
    }

    #[test]
    fn mistakes_name_their_line() {
        assert_eq!(error("print a\nif true\nprint b").line, 2);
        assert_eq!(error("print a\nend").line, 2);
        assert_eq!(error("else").line, 1);
        assert_eq!(error("\n\ndance b1").message, "unknown step `dance`");
        assert_eq!(error("\n\ndance b1").line, 3);
        assert_eq!(error("let = 3").line, 1);
        assert_eq!(error("let 1x = 3").line, 1);
        assert_eq!(error("for x [1]\nend").line, 1);
        assert_eq!(error("func (a)\nend").line, 1);
        assert_eq!(error("on b1 do\nend").line, 1);
        assert_eq!(error("sleep 1 2").line, 1);
        assert_eq!(error("break now").line, 1);
        assert_eq!(error("try\nprint a").line, 1);
        assert_eq!(error("wait").line, 1);
    }
}
