//! End-to-end tests for `yatr watch`, driving the real binary and real file events.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Watch {
    child: Child,
    out: Arc<Mutex<String>>,
}

impl Watch {
    fn start(dir: &Path, args: &[&str]) -> Self {
        Self::start_with(
            Command::new(assert_cmd::cargo::cargo_bin("yatr")),
            dir,
            args,
        )
    }

    fn start_with(mut cmd: Command, dir: &Path, args: &[&str]) -> Self {
        let mut child = cmd
            .current_dir(dir)
            .arg("watch")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self { child, out }
    }

    fn output(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    /// Wait until `needle` appears `count` times in stdout.
    fn wait_for(&self, needle: &str, count: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.output().matches(needle).count() >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn project(config: &str, files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), config).unwrap();
    for f in files {
        let path = dir.path().join(f);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "0").unwrap();
    }
    dir
}

/// Let the watcher settle after the initial run, then edit `file`.
fn edit(dir: &Path, file: &str) {
    std::thread::sleep(Duration::from_millis(700));
    std::fs::write(dir.join(file), "1").unwrap();
}

/// Regression (#20): a relative pattern never matched notify's absolute paths.
#[test]
fn relative_watch_pattern_triggers_rerun() {
    let dir = project(
        "[settings]\nwatch_debounce_ms = 50\n\
         [tasks.t]\nrun = [\"echo RAN\"]\nwatch = [\"src/*.txt\"]\n",
        &["src/a.txt"],
    );
    let w = Watch::start(dir.path(), &["t"]);
    assert!(w.wait_for("RAN", 1), "initial run: {}", w.output());
    edit(dir.path(), "src/a.txt");
    assert!(w.wait_for("RAN", 2), "no re-run on edit: {}", w.output());
}

/// Regression (#20): patterns resolve against the task's `cwd`.
#[test]
fn watch_patterns_resolve_against_task_cwd() {
    let dir = project(
        "[settings]\nwatch_debounce_ms = 50\n\
         [tasks.t]\nrun = [\"echo RAN\"]\ncwd = \"web\"\nsources = [\"src/**\"]\n",
        &["web/src/a.ts"],
    );
    let w = Watch::start(dir.path(), &["t"]);
    assert!(w.wait_for("RAN", 1), "initial run: {}", w.output());
    edit(dir.path(), "web/src/a.ts");
    assert!(w.wait_for("RAN", 2), "no re-run on edit: {}", w.output());
}

/// Regression (#20): `--clear` clears the screen before each run.
#[test]
fn clear_flag_clears_the_screen() {
    let dir = project(
        "[settings]\nwatch_debounce_ms = 50\n\
         [tasks.t]\nrun = [\"echo RAN\"]\nwatch = [\"*.txt\"]\n",
        &["a.txt"],
    );
    let w = Watch::start(dir.path(), &["--clear", "t"]);
    assert!(w.wait_for("RAN", 1), "initial run: {}", w.output());
    edit(dir.path(), "a.txt");
    assert!(w.wait_for("RAN", 2), "no re-run on edit: {}", w.output());
    // Windows clears through the console API, which a piped stdout does not have.
    #[cfg(unix)]
    assert!(
        w.output().matches("\x1b[2J").count() >= 2,
        "{:?}",
        w.output()
    );
}

/// Regression (#20): `[toolchain]` bins are on PATH in watch mode too.
#[cfg(unix)]
#[test]
fn watch_puts_toolchain_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = project(
        "[toolchain.fake]\nversion = \"1\"\nurl = \"https://invalid.example/fake.tgz\"\n\
         [tasks.t]\nrun = [\"fake-tool\"]\nwatch = [\"*.txt\"]\n",
        &[],
    );
    let tc = dir.path().join("tc");
    let install = tc.join("fake").join("1");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join(".yatr-installed"), "").unwrap();
    let tool = install.join("fake-tool");
    std::fs::write(&tool, "#!/bin/sh\necho FROM-TOOLCHAIN\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("yatr"));
    cmd.env("YATR_TOOLCHAIN_DIR", &tc);
    let w = Watch::start_with(cmd, dir.path(), &["t"]);
    assert!(w.wait_for("FROM-TOOLCHAIN", 1), "{}", w.output());
}
