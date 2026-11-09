//! Plugins: programs that wirbel starts with the session and that change it through the control
//! socket, without a change to wirbel itself. What they can ask for is in `plugin_api`, how to
//! write one in docs/PLUGINS.md.
//!
//! A plugin is an executable file, or a folder with an executable file `run` in it. The config
//! lists them as `plugin = PATH[, NAME][, true|false]`, and `wirbel plug` edits these lines:
//!
//! - `wirbel plug list` shows them, with the name if there is one.
//! - `wirbel plug add PATH [NAME]` adds one, switched on.
//! - `wirbel plug remove PATH|NAME` takes it out of the config.
//! - `wirbel plug PATH|NAME true|false` switches it on or off.
//! - `wirbel plug define PATH|NAME [NAME]` names it, or takes the name away.
//! - `wirbel plug create FOLDER` makes a folder ready for plugins: the Python helper `wirbel.py`
//!   and an example, without touching the config.
//!
//! After a change it asks a running wirbel to reload, which starts and stops plugins to match.
//! Every plugin runs in a process group of its own, which ends when it is switched off and with
//! the session. Its output goes to `~/.local/state/wirbel/plugins/NAME.log`.

use std::{
    env,
    fs::{self, File},
    io::Write,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    Wirbel,
    config::{Config, parse_bool},
    safety,
};

const PLUG_USAGE: &str = "usage: wirbel plug list
       wirbel plug create FOLDER
       wirbel plug add PATH [NAME] [PATH [NAME]]...
       wirbel plug remove PATH|NAME...
       wirbel plug PATH|NAME... true|false
       wirbel plug define PATH|NAME [NAME]";

/// Words of `wirbel plug` that a name must not be, or the command line would mean two things.
const RESERVED: &[&str] = &["add", "remove", "list", "define", "create", "help"];

/// The file a folder plugin starts with.
const FOLDER_ENTRY: &str = "run";

/// A `plugin =` line of the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    /// As written. `~/` stands for the home folder, a relative path starts there too.
    pub path: String,
    pub name: Option<String>,
    pub enabled: bool,
}

impl PluginEntry {
    pub fn parse(value: &str) -> Result<Self, String> {
        const USAGE: &str = "expected `plugin = PATH[, NAME][, true|false]`";
        let fields: Vec<&str> = value.split(',').map(str::trim).collect();
        let (path, name, enabled) = match fields.as_slice() {
            [path] => (*path, None, true),
            [path, second] => match parse_bool(second) {
                Ok(enabled) => (*path, None, enabled),
                Err(_) => (*path, Some(*second), true),
            },
            [path, name, enabled] => (*path, Some(*name), parse_bool(enabled)?),
            _ => return Err(USAGE.into()),
        };
        if path.is_empty() {
            return Err(USAGE.into());
        }
        let name = name.filter(|name| !name.is_empty());
        if let Some(name) = name {
            check_name(name)?;
        }
        Ok(Self {
            path: path.to_string(),
            name: name.map(str::to_string),
            enabled,
        })
    }

    /// The value of its config line.
    pub fn to_value(&self) -> String {
        match &self.name {
            Some(name) => format!("{}, {name}, {}", self.path, self.enabled),
            None => format!("{}, {}", self.path, self.enabled),
        }
    }

    /// The name, or the path if it has none.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.path)
    }

    /// Where it is on disk.
    pub fn location(&self) -> PathBuf {
        resolve(&self.path, home().as_deref())
    }
}

/// Names go where paths go on the command line. Without a slash in them, the two never mix up.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a name cannot be empty".into());
    }
    if name.contains(|c: char| c == ',' || c == '/' || c.is_whitespace()) {
        return Err(format!(
            "a name has no commas, slashes or spaces, got `{name}`"
        ));
    }
    if parse_bool(name).is_ok() || RESERVED.contains(&name) {
        return Err(format!(
            "`{name}` cannot be a name, wirbel plug uses that word"
        ));
    }
    Ok(())
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `path` with `~` expanded and relative to `base`, as real as the file system can make it.
fn resolve(path: &str, base: Option<&Path>) -> PathBuf {
    let expanded = match (path.strip_prefix('~'), home()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    };
    let absolute = match base {
        Some(base) if expanded.is_relative() => base.join(expanded),
        _ => expanded,
    };
    fs::canonicalize(&absolute).unwrap_or_else(|_| normalize(&absolute))
}

/// Takes out `.` and `..` without asking the file system, for paths that do not exist.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The file to run for the plugin at `path`.
pub fn executable(path: &Path) -> Result<PathBuf, String> {
    let shown = path.display();
    let meta = fs::metadata(path).map_err(|err| format!("{shown}: {err}"))?;
    let file = if meta.is_dir() {
        let file = path.join(FOLDER_ENTRY);
        if !file.is_file() {
            return Err(format!(
                "{shown} is a folder without a file `{FOLDER_ENTRY}`"
            ));
        }
        file
    } else {
        path.to_path_buf()
    };
    let meta = fs::metadata(&file).map_err(|err| format!("{}: {err}", file.display()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "{} is not executable, try chmod +x on it",
            file.display()
        ));
    }
    Ok(file)
}

/// A change to the plugin lines of a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Add(PluginEntry),
    /// By path or name, see `find`.
    Remove(String),
    Enable(String, bool),
    Define(String, Option<String>),
}

/// The plugin lines of a config, with their index.
fn plugin_lines(lines: &[String]) -> Vec<(usize, PluginEntry)> {
    let plugin = |line: &String| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == "plugin")
            .then(|| PluginEntry::parse(value.trim()).ok())
            .flatten()
    };
    let lines = lines.iter().enumerate();
    lines
        .filter_map(|(index, line)| Some((index, plugin(line)?)))
        .collect()
}

/// The entry that `id` means: one with that name, or else one at that path. A relative path is
/// relative to `cwd`.
fn find<'a>(
    entries: &'a [(usize, PluginEntry)],
    id: &str,
    cwd: Option<&Path>,
) -> Result<&'a (usize, PluginEntry), String> {
    let named = entries
        .iter()
        .find(|(_, entry)| entry.name.as_deref() == Some(id));
    if let Some(found) = named {
        return Ok(found);
    }
    let location = resolve(id, cwd);
    let mut at = entries
        .iter()
        .filter(|(_, entry)| entry.location() == location);
    match (at.next(), at.next()) {
        (Some(found), None) => Ok(found),
        (Some(_), Some(_)) => Err(format!("{id} is in the config more than once")),
        (None, _) => Err(format!("no plugin {id}, see wirbel plug list")),
    }
}

/// `text` with `edit` done to it. The other lines stay as they are. `cwd` is what a relative path
/// on the command line is relative to.
pub fn apply_edit(text: &str, edit: &Edit, cwd: Option<&Path>) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let entries = plugin_lines(&lines);
    let name_taken = |name: &str, except: Option<usize>| {
        let others = entries.iter().filter(|(index, _)| Some(*index) != except);
        let mut names = others.filter_map(|(_, entry)| entry.name.as_deref());
        match names.any(|other| other == name) {
            true => Err(format!("the name {name} is taken")),
            false => Ok(()),
        }
    };
    match edit {
        Edit::Add(entry) => {
            let location = entry.location();
            let same = entries
                .iter()
                .find(|(_, other)| other.location() == location);
            if let Some((_, other)) = same {
                return Err(format!("{} is added already", other.label()));
            }
            if let Some(name) = &entry.name {
                name_taken(name, None)?;
            }
            lines.push(format!("plugin = {}", entry.to_value()));
        }
        Edit::Remove(id) => {
            let (index, _) = find(&entries, id, cwd)?;
            lines.remove(*index);
        }
        Edit::Enable(id, enabled) => {
            let (index, entry) = find(&entries, id, cwd)?;
            let entry = PluginEntry {
                enabled: *enabled,
                ..entry.clone()
            };
            lines[*index] = format!("plugin = {}", entry.to_value());
        }
        Edit::Define(id, name) => {
            let (index, entry) = find(&entries, id, cwd)?;
            if let Some(name) = name {
                check_name(name)?;
                name_taken(name, Some(*index))?;
            }
            let entry = PluginEntry {
                name: name.clone(),
                ..entry.clone()
            };
            lines[*index] = format!("plugin = {}", entry.to_value());
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

/// The config `wirbel plug` works on: that of the running wirbel, which it hands to the programs it
/// starts, or the usual one.
fn config_file() -> Result<PathBuf, String> {
    env::var_os("WIRBEL_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(Config::default_path)
        .ok_or("cannot tell where the config is, HOME is not set".into())
}

/// Writes the config with `edits` done to it, one after the other. If one of them fails, none is
/// written. Writes through a symbolic link, so a config kept
/// with the dotfiles stays there, and replaces the file at once, so wirbel never reads half of it.
fn edit_file(path: &Path, edits: &[Edit]) -> Result<(), String> {
    let shown = path.display();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("{shown}: {err}")),
    };
    let cwd = env::current_dir().ok();
    let mut text = text;
    for edit in edits {
        text = apply_edit(&text, edit, cwd.as_deref())?;
    }
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(folder) = target.parent() {
        fs::create_dir_all(folder).map_err(|err| format!("{}: {err}", folder.display()))?;
    }
    let mut temporary = target.clone().into_os_string();
    temporary.push(".wirbel-plug");
    let temporary = PathBuf::from(temporary);
    let written = (|| {
        let mut file = File::create(&temporary)?;
        if let Ok(meta) = fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    written.map_err(|err| {
        let _ = fs::remove_file(&temporary);
        format!("{}: {err}", target.display())
    })
}

/// `wirbel plug`.
pub fn plug(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["create", folder] = args.as_slice() {
        let cwd = env::current_dir().ok();
        let folder = resolve(folder, cwd.as_deref());
        for (file, made) in create(&folder)? {
            let what = if made { "made" } else { "kept, it was there" };
            println!("{}: {what}", folder.join(file).display());
        }
        println!(
            "switch the example on with: wirbel plug add {} beispiel",
            folder.join("beispiel.py").display()
        );
        return Ok(());
    }
    let config = config_file()?;
    let edits = match args.as_slice() {
        [] | ["list"] => return list(&config),
        ["help" | "-h" | "--help"] => {
            println!("{PLUG_USAGE}");
            return Ok(());
        }
        ["add", rest @ ..] if !rest.is_empty() => {
            let cwd = env::current_dir().ok();
            let mut edits = Vec::new();
            for (path, name) in add_pairs(rest, cwd.as_deref()) {
                edits.push(add(path, name)?);
            }
            edits
        }
        ["remove", ids @ ..] if !ids.is_empty() => {
            ids.iter().map(|id| Edit::Remove(id.to_string())).collect()
        }
        ["define", id] => vec![Edit::Define(id.to_string(), None)],
        ["define", id, name] => vec![Edit::Define(id.to_string(), Some(name.to_string()))],
        [ids @ .., state] if !ids.is_empty() && !RESERVED.contains(&ids[0]) => {
            match parse_bool(state) {
                Ok(enabled) => ids
                    .iter()
                    .map(|id| Edit::Enable(id.to_string(), enabled))
                    .collect(),
                Err(_) => return Err(PLUG_USAGE.into()),
            }
        }
        _ => return Err(PLUG_USAGE.into()),
    };
    edit_file(&config, &edits)?;
    for edit in &edits {
        println!("{}", done(edit));
    }
    reload(&config);
    Ok(())
}

/// The arguments of `wirbel plug add`, split into paths, each with the name after it if there is
/// one. A word is a name when it has no slash and is not a file or folder in `cwd`, otherwise it is
/// the next path. So `add a.py wackeln b.py` adds two, the first named, and `add *.py` adds them all.
pub fn add_pairs<'a>(args: &[&'a str], cwd: Option<&Path>) -> Vec<(&'a str, Option<&'a str>)> {
    let is_name = |word: &str| !word.contains('/') && !resolve(word, cwd).exists();
    let mut pairs = Vec::new();
    let mut words = args.iter().copied().peekable();
    while let Some(path) = words.next() {
        let name = words.next_if(|word| is_name(word));
        pairs.push((path, name));
    }
    pairs
}

/// What `wirbel plug create` puts into a folder: name, content, executable.
const TEMPLATE: &[(&str, &str, bool)] = &[
    ("wirbel.py", include_str!("../plugins/wirbel.py"), false),
    ("beispiel.py", include_str!("../plugins/beispiel.py"), true),
];

/// Makes `folder` (and the folders above it) if needed and writes the template files into it.
/// A file that is already there stays as it is. Says for each file whether it was made.
pub fn create(folder: &Path) -> Result<Vec<(&'static str, bool)>, String> {
    let shown = folder.display();
    if folder.exists() && !folder.is_dir() {
        return Err(format!("{shown} is not a folder"));
    }
    fs::create_dir_all(folder).map_err(|err| format!("{shown}: {err}"))?;
    let mut report = Vec::new();
    for &(name, content, executable) in TEMPLATE {
        let path = folder.join(name);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        let mut file = match file {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                report.push((name, false));
                continue;
            }
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        file.write_all(content.as_bytes())
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .map_err(|err| format!("{}: {err}", path.display()))?;
        report.push((name, true));
    }
    Ok(report)
}

/// What `wirbel plug` says about an edit it made.
fn done(edit: &Edit) -> String {
    let on = |enabled: bool| if enabled { "on" } else { "off" };
    match edit {
        Edit::Add(PluginEntry {
            path,
            name: Some(name),
            ..
        }) => format!("added {name} ({path}), on"),
        Edit::Add(entry) => format!("added {}, on", entry.path),
        Edit::Remove(id) => format!("removed {id}"),
        Edit::Enable(id, enabled) => format!("{id} is {}", on(*enabled)),
        Edit::Define(id, Some(name)) => format!("{id} is called {name} now"),
        Edit::Define(id, None) => format!("{id} has no name now"),
    }
}

fn add(path: &str, name: Option<&str>) -> Result<Edit, String> {
    let cwd = env::current_dir().ok();
    let location = resolve(path, cwd.as_deref());
    executable(&location)?;
    let path = location
        .to_str()
        .ok_or(format!("{} is not UTF-8", location.display()))?;
    if path.contains(',') {
        return Err(format!("{path} has a comma, which the config cannot hold"));
    }
    if let Some(name) = name {
        check_name(name)?;
    }
    Ok(Edit::Add(PluginEntry {
        path: path.to_string(),
        name: name.map(str::to_string),
        enabled: true,
    }))
}

/// Tells a running wirbel to read the config again, and says if it cannot.
fn reload(config: &Path) {
    if let Ok((_, errors)) = Config::load(config)
        && let Some(error) = errors.first()
    {
        eprintln!(
            "wirbel plug: the config has an error, so wirbel keeps the old one until it is fixed: {error}"
        );
        return;
    }
    match crate::cli::ask_lines("reload") {
        Ok(_) => {}
        Err(_) => println!("wirbel is not running here, this counts from its next start"),
    }
}

/// `wirbel plug list`: on or off, the name if there is one, the path, and whether it runs.
fn list(config: &Path) -> Result<(), String> {
    let (config, errors) =
        Config::load(config).map_err(|err| format!("{}: {err}", config.display()))?;
    for error in errors
        .iter()
        .filter(|error| error.message.contains("plugin"))
    {
        eprintln!("wirbel plug: skipping a config line, {error}");
    }
    if config.plugins.is_empty() {
        println!("no plugins, add one with wirbel plug add PATH");
        return Ok(());
    }
    // Only a wirbel that knows about plugins answers this.
    let running: Option<Vec<PathBuf>> = crate::cli::ask_lines("plugins").ok().map(|lines| {
        let paths = lines
            .iter()
            .filter_map(|line| line.strip_prefix("running "));
        paths.map(PathBuf::from).collect()
    });
    let width = config
        .plugins
        .iter()
        .filter_map(|entry| entry.name.as_ref())
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    for entry in &config.plugins {
        let state = if entry.enabled { "on " } else { "off" };
        let name = match &entry.name {
            Some(name) => format!("{name:width$}  "),
            None => String::new(),
        };
        let runs = match &running {
            Some(running) if entry.enabled => match running.contains(&entry.location()) {
                true => "  running",
                false => "  not running, see its log in ~/.local/state/wirbel/plugins",
            },
            _ => "",
        };
        println!("{state}  {name}{}{runs}", entry.path);
    }
    Ok(())
}

/// A plugin that wirbel started.
struct Running {
    location: PathBuf,
    label: String,
    group: libc::pid_t,
    /// False once the process ended.
    alive: Arc<AtomicBool>,
}

/// The plugins that run.
#[derive(Default)]
pub struct PluginHost {
    running: Vec<Running>,
}

impl PluginHost {
    /// Where the running plugins are.
    pub fn running(&self) -> impl Iterator<Item = &Path> {
        let alive = self
            .running
            .iter()
            .filter(|plugin| plugin.alive.load(Ordering::Relaxed));
        alive.map(|plugin| plugin.location.as_path())
    }
}

/// Where the output of a plugin goes. Each start begins it anew.
fn plugin_log(label: &str) -> Option<File> {
    let folder = safety::default_log_path()?.parent()?.join("plugins");
    fs::create_dir_all(&folder).ok()?;
    let name: String = label
        .trim_start_matches('/')
        .chars()
        .map(|c| if c == '/' { '_' } else { c })
        .collect();
    File::create(folder.join(format!("{name}.log"))).ok()
}

impl Wirbel {
    /// Starts the plugins that are switched on and do not run, and ends those that are off or gone
    /// from the config. A plugin that ended by itself starts again.
    pub fn sync_plugins(&mut self) {
        let wanted: Vec<(PathBuf, String)> = self
            .config
            .plugins
            .iter()
            .filter(|entry| entry.enabled)
            .map(|entry| (entry.location(), entry.label().to_string()))
            .collect();
        self.plugins.running.retain(|plugin| {
            let alive = plugin.alive.load(Ordering::Relaxed);
            let keep = alive
                && wanted
                    .iter()
                    .any(|(location, _)| *location == plugin.location);
            if alive && !keep {
                tracing::info!(plugin = plugin.label, "stopping plugin");
                // Safety: the group is ours, the reaper forgets it once it is empty.
                unsafe { libc::kill(-plugin.group, libc::SIGTERM) };
            }
            keep
        });
        for (location, label) in wanted {
            let running = self.plugins.running.iter_mut();
            match running
                .into_iter()
                .find(|plugin| plugin.location == location)
            {
                Some(plugin) => plugin.label = label,
                None => self.start_plugin(location, label),
            }
        }
    }

    fn start_plugin(&mut self, location: PathBuf, label: String) {
        let file = match executable(&location) {
            Ok(file) => file,
            Err(err) => {
                tracing::error!(plugin = label, %err, "plugin not started");
                return;
            }
        };
        let mut child = Command::new(&file);
        child
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("WIRBEL_PLUGIN", &label)
            .env_remove(safety::SESSION_ENV)
            .stdin(Stdio::null())
            .process_group(0);
        if let Some(folder) = file.parent() {
            child.current_dir(folder);
        }
        if let Some(socket) = self.ipc.path() {
            child.env("WIRBEL_SOCKET", socket);
        }
        if let Some(config) = &self.config_path {
            child.env("WIRBEL_CONFIG", config);
        }
        match self.xwayland.display {
            Some(display) => child.env("DISPLAY", format!(":{display}")),
            None => child.env_remove("DISPLAY"),
        };
        if let Some(log) = plugin_log(&label) {
            if let Ok(copy) = log.try_clone() {
                child.stdout(copy);
            }
            child.stderr(log);
        }
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(err) => {
                tracing::error!(plugin = label, %err, "plugin not started");
                return;
            }
        };
        let group = child.id() as libc::pid_t;
        safety::end_at_exit(group);
        tracing::info!(plugin = label, file = %file.display(), "plugin started");
        let alive = Arc::new(AtomicBool::new(true));
        let (flag, name) = (alive.clone(), label.clone());
        std::thread::spawn(move || {
            let status = child.wait();
            flag.store(false, Ordering::Relaxed);
            safety::group_left(group);
            match status {
                Ok(status) if status.success() => tracing::info!(plugin = name, "plugin ended"),
                Ok(status) => tracing::warn!(plugin = name, %status, "plugin ended"),
                Err(err) => tracing::warn!(plugin = name, %err, "lost track of a plugin"),
            }
        });
        self.plugins.running.push(Running {
            location,
            label,
            group,
            alive,
        });
    }
}

/// Reads lines up to the `ok` that ends a list. Lines are `None` once the socket ends.
pub(crate) fn read_list(
    mut next: impl FnMut() -> Result<Option<String>, String>,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    loop {
        match next()? {
            Some(line) if line == "ok" => return Ok(lines),
            Some(line) if line.starts_with("error ") => return Err(line[6..].to_string()),
            Some(line) => lines.push(line),
            None => return Err("the compositor went away".into()),
        }
    }
}

/// Unused outside of tests, which feed `read_list` from memory.
#[cfg(test)]
fn lines_of(text: &str) -> impl FnMut() -> Result<Option<String>, String> + '_ {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(text.as_bytes());
    move || {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line.trim_end().to_string())),
            Err(err) => Err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, name: Option<&str>, enabled: bool) -> PluginEntry {
        PluginEntry {
            path: path.into(),
            name: name.map(Into::into),
            enabled,
        }
    }

    #[test]
    fn create_makes_missing_folders_and_keeps_existing_files() {
        let root = env::temp_dir().join(format!("wirbel-create-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let folder = root.join("a/b");
        let report = create(&folder).unwrap();
        assert_eq!(report, vec![("wirbel.py", true), ("beispiel.py", true)]);
        assert!(executable(&folder.join("beispiel.py")).is_ok());
        fs::write(folder.join("wirbel.py"), "mine").unwrap();
        fs::remove_file(folder.join("beispiel.py")).unwrap();
        let report = create(&folder).unwrap();
        assert_eq!(report, vec![("wirbel.py", false), ("beispiel.py", true)]);
        assert_eq!(
            fs::read_to_string(folder.join("wirbel.py")).unwrap(),
            "mine"
        );
        assert!(create(&folder.join("beispiel.py")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_takes_several_paths_each_with_an_optional_name() {
        let folder = std::env::temp_dir().join(format!("wirbel-add-pairs-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("b.py"), "").unwrap();
        let pairs = add_pairs(
            &["a.py", "eins", "b.py", "/x/c", "zwei", "/x/d"],
            Some(&folder),
        );
        assert_eq!(
            pairs,
            [
                ("a.py", Some("eins")),
                ("b.py", None),
                ("/x/c", Some("zwei")),
                ("/x/d", None)
            ]
        );
        assert_eq!(
            add_pairs(&["/x/a", "/x/b"], None),
            [("/x/a", None), ("/x/b", None)]
        );
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn lines_take_a_name_and_a_state_in_any_combination() {
        let parse = |value| PluginEntry::parse(value).unwrap();
        assert_eq!(parse("/a/b"), entry("/a/b", None, true));
        assert_eq!(parse("/a/b, off"), entry("/a/b", None, false));
        assert_eq!(
            parse("/a/b,confetti"),
            entry("/a/b", Some("confetti"), true)
        );
        assert_eq!(
            parse("/a/b, confetti, false"),
            entry("/a/b", Some("confetti"), false)
        );
        assert_eq!(parse("/a/b, , 1"), entry("/a/b", None, true));
        for bad in [
            "",
            ", x",
            "/a, x, maybe",
            "/a, x y",
            "/a, list",
            "/a, true, true",
            "/a,b,1,2",
        ] {
            assert!(PluginEntry::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn lines_survive_a_round_trip() {
        for original in [entry("/a b/c", Some("x"), false), entry("~/p", None, true)] {
            assert_eq!(PluginEntry::parse(&original.to_value()).unwrap(), original);
        }
    }

    #[test]
    fn names_never_look_like_paths_or_words() {
        assert!(check_name("wobble-2").is_ok());
        for bad in ["", "a/b", "a,b", "a b", "on", "0", "list", "define"] {
            assert!(check_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn edits_touch_only_plugin_lines() {
        let text = "# mine\ngap_outer = 3\nplugin = /p/a, alpha, true\n\nplugin=/p/b\n";
        let edit = |text: &str, edit| apply_edit(text, &edit, None).unwrap();

        let added = edit(text, Edit::Add(entry("/p/c", None, true)));
        assert_eq!(added, format!("{text}plugin = /p/c, true\n"));

        let off = edit(text, Edit::Enable("alpha".into(), false));
        assert_eq!(off, text.replace("/p/a, alpha, true", "/p/a, alpha, false"));
        let off = edit(text, Edit::Enable("/p/b".into(), false));
        assert_eq!(off, text.replace("plugin=/p/b", "plugin = /p/b, false"));

        let named = edit(text, Edit::Define("/p/b".into(), Some("beta".into())));
        assert!(named.ends_with("plugin = /p/b, beta, true\n"));
        let renamed = edit(&named, Edit::Enable("beta".into(), false));
        assert!(renamed.ends_with("plugin = /p/b, beta, false\n"));
        let unnamed = edit(text, Edit::Define("alpha".into(), None));
        assert!(unnamed.contains("plugin = /p/a, true\n"));

        let removed = edit(text, Edit::Remove("alpha".into()));
        assert_eq!(removed, "# mine\ngap_outer = 3\n\nplugin=/p/b\n");
        assert_eq!(
            edit("", Edit::Add(entry("/x", Some("x"), true))),
            "plugin = /x, x, true\n"
        );
    }

    #[test]
    fn edits_refuse_what_would_make_a_mess() {
        let text = "plugin = /p/a, alpha, true\nplugin = /p/b, true\n";
        let fails = |edit| apply_edit(text, &edit, None).unwrap_err();
        assert!(fails(Edit::Add(entry("/p/a", None, true))).contains("added already"));
        assert!(fails(Edit::Add(entry("/p/./a", None, true))).contains("added already"));
        assert!(fails(Edit::Add(entry("/p/c", Some("alpha"), true))).contains("taken"));
        assert!(fails(Edit::Define("/p/b".into(), Some("alpha".into()))).contains("taken"));
        assert!(fails(Edit::Define("/p/b".into(), Some("a b".into()))).contains("spaces"));
        assert!(fails(Edit::Remove("gamma".into())).contains("no plugin gamma"));
        // Giving a plugin the name it has is no change, but no mistake either.
        assert!(
            apply_edit(
                text,
                &Edit::Define("alpha".into(), Some("alpha".into())),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn relative_ids_count_from_the_working_folder() {
        let text = "plugin = /p/a, true\n";
        let edit = Edit::Enable("a".into(), false);
        let off = apply_edit(text, &edit, Some(Path::new("/p"))).unwrap();
        assert_eq!(off, "plugin = /p/a, false\n");
        assert!(apply_edit(text, &edit, Some(Path::new("/q"))).is_err());
    }

    #[test]
    fn executables_are_files_with_an_x_or_folders_with_run() {
        let folder = env::temp_dir().join(format!("wirbel-plugin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(folder.join("tree")).unwrap();
        let plain = folder.join("plain");
        fs::write(&plain, "#!/bin/sh\n").unwrap();
        assert!(executable(&plain).unwrap_err().contains("chmod"));
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(executable(&plain).unwrap(), plain);
        assert!(
            executable(&folder.join("tree"))
                .unwrap_err()
                .contains("run")
        );
        let run = folder.join("tree").join("run");
        fs::write(&run, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(executable(&folder.join("tree")).unwrap(), run);
        assert!(executable(&folder.join("missing")).is_err());
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn lists_end_at_ok_and_fail_at_an_error() {
        assert_eq!(
            read_list(lines_of("running /a\nrunning /b\nok\n")).unwrap(),
            ["running /a", "running /b"]
        );
        assert_eq!(
            read_list(lines_of("error unknown request\n")).unwrap_err(),
            "unknown request"
        );
        assert!(read_list(lines_of("running /a\n")).is_err());
    }
}
