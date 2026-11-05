//! Finding Xwayland. wirbel runs in one Bedrock stratum and its clients come from another, so the
//! binary may be anywhere: on the `PATH`, in the cross directory, or only in another stratum. Each
//! candidate has to prove that it runs here: a binary of another stratum may be visible, but lack
//! its libraries.

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const NAME: &str = "Xwayland";
/// Inside a stratum. Plain `Xwayland` would find the shim again.
const IN_STRATUM: &str = "/usr/bin/Xwayland";
const CROSS: &str = "/bedrock/cross/bin/Xwayland";
const STRATA: &str = "/bedrock/strata";
const STRAT: &str = "/bedrock/bin/strat";

/// A way to run Xwayland: the program, and what goes before its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binary(pub Vec<String>);

impl Binary {
    fn of(words: &[&str]) -> Self {
        Self(words.iter().map(ToString::to_string).collect())
    }

    /// Whether this is what running `Xwayland` from the `PATH` gets anyway.
    fn is_plain(&self) -> bool {
        self.0.len() == 1 && self.0[0] == NAME
    }

    fn runs(&self) -> bool {
        let Some((program, args)) = self.0.split_first() else {
            return false;
        };
        Command::new(program)
            .args(args)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Smithay only ever runs `Xwayland` from the `PATH`. A script of that name in a directory of
    /// its own makes that this binary. The shell reads the script after the spawn, so it has to
    /// stay until the server is up or dead.
    pub fn shim(&self, directory: &Path) -> io::Result<Option<OsPath>> {
        if self.is_plain() {
            return Ok(None);
        }
        fs::create_dir_all(directory)?;
        let script = directory.join(NAME);
        let quoted: Vec<String> = self.0.iter().map(|word| shell_quote(word)).collect();
        fs::write(
            &script,
            format!("#!/bin/sh\nexec {} \"$@\"\n", quoted.join(" ")),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;

        let mut paths = vec![directory.to_path_buf()];
        paths.extend(env::var_os("PATH").iter().flat_map(env::split_paths));
        let path = env::join_paths(paths).map_err(io::Error::other)?;
        Ok(Some(OsPath(path)))
    }
}

/// A value for `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsPath(pub std::ffi::OsString);

fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Where to look, in order. `Xwayland` of the own stratum comes first, it shares the graphics
/// libraries of the compositor. The Arch stratum goes before other strata because the clients
/// come from there.
fn candidates(strata: &[String]) -> Vec<Binary> {
    let mut list = vec![Binary::of(&[NAME]), Binary::of(&[CROSS])];
    let mut strata: Vec<&str> = strata.iter().map(String::as_str).collect();
    strata.sort_by_key(|stratum| *stratum != "arch");
    list.extend(
        strata
            .into_iter()
            .map(|s| Binary::of(&[STRAT, s, IN_STRATUM])),
    );
    list
}

fn strata_with_xwayland() -> Vec<String> {
    let Ok(entries) = fs::read_dir(STRATA) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            let path: PathBuf = entry.path().join("usr/bin").join(NAME);
            path.exists()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The Xwayland to run: `configured`, or the first candidate that works. A configured path that
/// does not work is an error of the user and gets no replacement, but a word in the log.
pub fn locate(configured: Option<&str>) -> Option<Binary> {
    if let Some(path) = configured {
        let binary = Binary::of(&[path]);
        if binary.runs() {
            return Some(binary);
        }
        tracing::error!(path, "the configured xwayland does not run");
        return None;
    }
    let strata = strata_with_xwayland();
    candidates(&strata).into_iter().find(Binary::runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_stratum_goes_first_then_arch() {
        let strata = ["debian".to_string(), "arch".to_string()];
        let list = candidates(&strata);
        assert!(list[0].is_plain());
        assert_eq!(list[1], Binary::of(&[CROSS]));
        assert_eq!(list[2], Binary::of(&[STRAT, "arch", IN_STRATUM]));
        assert_eq!(list[3], Binary::of(&[STRAT, "debian", IN_STRATUM]));
    }

    /// The shim stays on the `PATH`, so a plain name would land in it again, forever.
    #[test]
    fn every_candidate_but_the_first_names_its_binary_in_full() {
        let list = candidates(&["arch".to_string()]);
        for binary in &list[1..] {
            assert!(binary.0.last().unwrap().starts_with('/'), "{binary:?}");
        }
    }

    #[test]
    fn a_plain_binary_needs_no_shim() {
        let directory = env::temp_dir().join("wirbel-no-shim");
        assert_eq!(Binary::of(&[NAME]).shim(&directory).unwrap(), None);
        assert!(!directory.exists());
    }

    #[test]
    fn a_program_that_fails_does_not_count() {
        assert!(!Binary::of(&["false"]).runs());
        assert!(!Binary::of(&["/nonexistent/Xwayland"]).runs());
        assert_eq!(locate(Some("/nonexistent/Xwayland")), None);
    }

    #[test]
    fn the_shim_passes_on_arguments_and_survives_odd_paths() {
        let directory = env::temp_dir().join(format!("wirbel-shim-{}", std::process::id()));
        let binary = Binary::of(&["/bin/echo", "it's"]);
        let path = binary.shim(&directory).unwrap().unwrap();
        let output = Command::new(NAME)
            .env("PATH", path.0)
            .args(["a b", "c"])
            .output()
            .unwrap();
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's a b c\n");
    }
}
