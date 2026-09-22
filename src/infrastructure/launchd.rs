//! The launchd LaunchAgent that keeps a queue's supervisor resident. `up`
//! writes one plist per queue under `~/Library/LaunchAgents` and loads it
//! into the user's `gui/<uid>` domain; `down` unloads it. launchd restarts
//! the supervisor whenever it exits (`KeepAlive`), which is why stopping it
//! goes through `bootout` rather than a signal.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::adapters::capture;
use crate::application::{AgentState, LaunchAgent};

/// How long launchd waits after SIGTERM before it kills the supervisor. A
/// drain waits for the active runs, which can take as long as their
/// sessions, so the default (20 s) is far too short.
pub const EXIT_TIMEOUT_SECS: u32 = 86_400;

/// Everything that goes into the plist, kept as data so tests can check it
/// without parsing XML.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchAgentSpec {
    pub label: String,
    pub plist: PathBuf,
    pub program_arguments: Vec<String>,
    pub working_directory: String,
    /// PATH of the shell that ran `up`; launchd's own is too small for
    /// `cmux` and `claude`.
    pub path: String,
    pub log: String,
}

impl LaunchAgentSpec {
    /// The plist as launchd reads it: `KeepAlive` and `RunAtLoad` so the
    /// supervisor starts now and restarts after any exit, stdout and stderr
    /// appended to one file, and a long `ExitTimeOut` for the drain.
    pub fn xml(&self) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        xml.push_str(&format!(
            "\t<key>Label</key>\n\t<string>{}</string>\n",
            escape(&self.label)
        ));
        xml.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        for argument in &self.program_arguments {
            xml.push_str(&format!("\t\t<string>{}</string>\n", escape(argument)));
        }
        xml.push_str("\t</array>\n");
        xml.push_str(&format!(
            "\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n",
            escape(&self.working_directory)
        ));
        xml.push_str(&format!(
            "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>{}</string>\n\t</dict>\n",
            escape(&self.path)
        ));
        xml.push_str("\t<key>KeepAlive</key>\n\t<true/>\n");
        xml.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
        xml.push_str(&format!(
            "\t<key>ExitTimeOut</key>\n\t<integer>{EXIT_TIMEOUT_SECS}</integer>\n"
        ));
        xml.push_str(&format!(
            "\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n",
            log = escape(&self.log)
        ));
        xml.push_str("</dict>\n</plist>\n");
        xml
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `launchctl` against the `gui/<uid>` domain of the user running `up`.
pub struct Launchctl {
    pub uid: u32,
}

/// How long `install` waits for a previously loaded agent's process to
/// go away after its bootout before forcing it, and again after that.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(60);
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(30);

impl Launchctl {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self, label: &str) -> String {
        format!("{}/{label}", self.domain())
    }

    /// The service as launchd sees it: loaded (still listed) and its pid.
    /// A service stays listed after `bootout` until its process has exited.
    fn state(&self, label: &str) -> Result<AgentState> {
        let (status, stdout, _) = capture(
            Command::new("launchctl").args(["print", &self.target(label)]),
            LAUNCHCTL_TIMEOUT,
        )?;
        if !status.success() {
            return Ok(AgentState {
                loaded: false,
                pid: None,
            });
        }
        Ok(AgentState {
            loaded: true,
            pid: print_pid(&stdout),
        })
    }

    /// Ask launchd to remove the service; it returns at once and the
    /// process gets SIGTERM. `Ok(false)` when nothing was loaded.
    fn bootout(&self, label: &str) -> Result<bool> {
        let target = self.target(label);
        let (status, _, stderr) = capture(
            Command::new("launchctl").args(["bootout", &target]),
            LAUNCHCTL_TIMEOUT,
        )?;
        if status.success() {
            return Ok(true);
        }
        // 3 (ESRCH) and 113 are launchctl's "no such service" replies.
        if matches!(status.code(), Some(3) | Some(113))
            || stderr.contains("Could not find service")
            || stderr.contains("No such process")
        {
            return Ok(false);
        }
        anyhow::bail!("launchctl bootout {target} failed ({status}): {stderr}");
    }

    /// Wait until the service is gone, forcing it with SIGKILL once the
    /// timeout passes (a hung supervisor would otherwise hold the label
    /// for launchd's `ExitTimeOut`).
    fn wait_unloaded(&self, label: &str) -> Result<()> {
        let started = Instant::now();
        let mut killed = false;
        loop {
            if !self.state(label)?.loaded {
                return Ok(());
            }
            if started.elapsed() >= REPLACE_TIMEOUT * if killed { 2 } else { 1 } {
                ensure!(
                    !killed,
                    "agent {label} did not unload within {}s even after SIGKILL",
                    2 * REPLACE_TIMEOUT.as_secs()
                );
                let _ = capture(
                    Command::new("launchctl").args(["kill", "SIGKILL", &self.target(label)]),
                    LAUNCHCTL_TIMEOUT,
                );
                killed = true;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The `pid = N` line of `launchctl print`.
pub fn print_pid(listing: &str) -> Option<u32> {
    listing
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

impl LaunchAgent for Launchctl {
    fn install(&self, label: &str, path: &Path, contents: &str) -> Result<()> {
        ensure!(
            path.is_absolute(),
            "LaunchAgent path {} must be absolute (HOME is unset?)",
            path.display()
        );
        let dir = path.parent().context("LaunchAgent path has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
        // A definition already loaded keeps its old arguments until reloaded,
        // and launchd refuses a bootstrap while the old service is exiting.
        if self.bootout(label)? {
            self.wait_unloaded(label)?;
        }
        let (status, _, stderr) = capture(
            Command::new("launchctl")
                .args(["bootstrap", &self.domain()])
                .arg(path),
            LAUNCHCTL_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "launchctl bootstrap {} {} failed ({status}): {stderr}",
            self.domain(),
            path.display()
        );
        Ok(())
    }

    fn uninstall(&self, label: &str, path: &Path) -> Result<AgentState> {
        let state = self.state(label)?;
        if state.loaded {
            self.bootout(label)?;
        }
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_lists_every_key_launchd_needs() {
        let spec = LaunchAgentSpec {
            label: "com.cmux-taskq.abc".into(),
            plist: "/home/u/Library/LaunchAgents/com.cmux-taskq.abc.plist".into(),
            program_arguments: vec![
                "/bin/cmux-taskq".into(),
                "--db".into(),
                "/data/q/queue.db".into(),
                "supervise".into(),
                "--parallel".into(),
                "3".into(),
                "--log-dir".into(),
                "/data/q/logs".into(),
            ],
            working_directory: "/repo & co".into(),
            path: "/usr/bin:/home/u/.local/bin".into(),
            log: "/data/q/logs/launchd.log".into(),
        };
        let xml = spec.xml();
        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains("<key>Label</key>\n\t<string>com.cmux-taskq.abc</string>"));
        assert!(xml.contains(
            "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/bin/cmux-taskq</string>\n\t\t<string>--db</string>"
        ));
        assert!(xml.contains("<string>--parallel</string>\n\t\t<string>3</string>\n\t\t<string>--log-dir</string>\n\t\t<string>/data/q/logs</string>\n\t</array>"));
        assert!(xml.contains("<key>WorkingDirectory</key>\n\t<string>/repo &amp; co</string>"));
        assert!(xml.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/home/u/.local/bin</string>\n\t</dict>"
        ));
        assert!(xml.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(xml.contains("<key>ExitTimeOut</key>\n\t<integer>86400</integer>"));
        assert!(
            xml.contains("<key>StandardOutPath</key>\n\t<string>/data/q/logs/launchd.log</string>")
        );
        assert!(
            xml.contains(
                "<key>StandardErrorPath</key>\n\t<string>/data/q/logs/launchd.log</string>"
            )
        );
        assert!(xml.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    fn uninstall_tolerates_a_missing_plist_but_not_a_relative_install_path() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: getuid has no preconditions.
        let launchctl = Launchctl {
            uid: unsafe { libc::getuid() },
        };
        let error = launchctl
            .install("x", Path::new("relative.plist"), "")
            .unwrap_err();
        assert!(format!("{error:#}").contains("must be absolute"));
        // Nothing is loaded under a label that was never bootstrapped, and a
        // plist that is not there is not an error either.
        let missing = dir.path().join("com.cmux-taskq.missing.plist");
        assert_eq!(
            launchctl
                .uninstall("com.cmux-taskq.missing", &missing)
                .unwrap(),
            AgentState {
                loaded: false,
                pid: None
            }
        );
        assert!(!launchctl.bootout("com.cmux-taskq.missing").unwrap());
    }

    #[test]
    fn print_pid_reads_the_pid_line() {
        let listing = "gui/501/com.cmux-taskq.x = {\n\tactive count = 1\n\tpath = /p\n\tstate = running\n\n\tpid = 4213\n\tprogram = /bin\n}";
        assert_eq!(print_pid(listing), Some(4213));
        assert_eq!(
            print_pid("gui/501/x = {\n\tstate = spawn scheduled\n}"),
            None
        );
    }
}
