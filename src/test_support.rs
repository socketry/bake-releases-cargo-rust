// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use bake::{Context, Registry};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use tempfile::TempDir;

pub struct Project {
    directory: TempDir,
}

impl Project {
    pub fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    pub fn root(&self) -> &Path {
        self.directory.path()
    }

    pub fn context(&self) -> Context {
        Registry::new().context(self.root())
    }

    pub fn write(&self, path: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.root().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    pub fn single_package(&self, name: &str, version: &str) {
        self.write(
            "Cargo.toml",
            &format!("[package]\nname = {name:?}\nversion = {version:?}\nedition = \"2024\"\n"),
        );
        self.write("src/lib.rs", "// fixture\n");
    }

    pub fn executable(&self, name: &str, script: &str) -> PathBuf {
        let path = self.root().join("bin").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    pub fn cargo_proxy(&self, environment: &mut Environment, fail: Option<&str>) -> PathBuf {
        let real_cargo = cargo_path(environment.original("CARGO"));
        let log = self.root().join("cargo-arguments.log");
        let failure = fail.unwrap_or("");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BAKE_TEST_CARGO_LOG\"\nif [ \"$1\" = metadata ]; then exec {} \"$@\"; fi\nif [ \"$1\" = \"$BAKE_TEST_CARGO_FAILURE\" ]; then echo 'fake cargo failure' >&2; exit 7; fi\nexit 0\n",
            shell_quote(&real_cargo)
        );
        let path = self.executable("cargo-proxy", &script);
        environment.set("CARGO", path.as_os_str());
        environment.set("BAKE_TEST_CARGO_LOG", log.as_os_str());
        environment.set("BAKE_TEST_CARGO_FAILURE", failure);
        path
    }

    pub fn cargo_arguments(&self) -> String {
        fs::read_to_string(self.root().join("cargo-arguments.log")).unwrap_or_default()
    }
}

fn cargo_path(value: Option<OsString>) -> PathBuf {
    value
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

/// Serialize tests that temporarily alter process environment variables.
pub struct Environment {
    _lock: MutexGuard<'static, ()>,
    original: HashMap<OsString, Option<OsString>>,
}

impl Environment {
    pub fn new() -> Self {
        Self {
            _lock: ENVIRONMENT_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            original: HashMap::new(),
        }
    }

    pub fn set(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        let name = name.as_ref().to_owned();
        self.original
            .entry(name.clone())
            .or_insert_with(|| std::env::var_os(&name));
        // The environment lock keeps changes from racing with other tests in this crate.
        unsafe { std::env::set_var(name, value) };
    }

    pub fn original(&self, name: impl AsRef<OsStr>) -> Option<OsString> {
        match self.original.get(name.as_ref()) {
            Some(value) => value.clone(),
            None => std::env::var_os(name),
        }
    }

    pub fn remove(&mut self, name: impl AsRef<OsStr>) {
        let name = name.as_ref().to_owned();
        self.original
            .entry(name.clone())
            .or_insert_with(|| std::env::var_os(&name));
        // The environment lock keeps changes from racing with other tests in this crate.
        unsafe { std::env::remove_var(name) };
    }

    pub fn prepend_path(&mut self, directory: &Path) {
        let mut paths = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .collect::<Vec<_>>();
        paths.insert(0, directory.to_owned());
        self.set("PATH", std::env::join_paths(paths).unwrap());
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in self.original.drain() {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

pub fn http_server(responses: Vec<(u16, String)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let headers = String::from_utf8_lossy(&request);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut request_body = vec![0; content_length];
            stream.read_exact(&mut request_body).unwrap();
            request.extend_from_slice(&request_body);
            requests.push(String::from_utf8_lossy(&request).into_owned());
            let reason = match status {
                200 => "OK",
                404 => "Not Found",
                _ => "Error",
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        requests
    });

    (format!("http://{address}"), thread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_shell_quotes_project_test_fixtures() {
        let project = Project::default();
        let _environment = Environment::default();

        assert!(project.root().exists());
        assert_eq!(
            shell_quote(Path::new("path/with 'quote")),
            "'path/with '\\''quote'"
        );
    }

    #[test]
    fn cargo_path_uses_the_path_fallback_when_unset() {
        assert_eq!(cargo_path(None), PathBuf::from("cargo"));
        assert_eq!(
            cargo_path(Some(OsString::from("/usr/bin/cargo"))),
            PathBuf::from("/usr/bin/cargo")
        );
    }

    #[test]
    fn original_reads_a_previously_changed_environment_variable() {
        let mut environment = Environment::new();
        environment.remove("BAKE_TEST_ORIGINAL");
        environment.set("BAKE_TEST_ORIGINAL", "changed");

        assert_eq!(environment.original("BAKE_TEST_ORIGINAL"), None);
    }
}
