//! The [`Spawner`] port on `std::process`: a [`CommandSpec`] becomes a
//! `Command`, started with the streams the caller asked for.

use std::{
    fs,
    process::{Child, Command, ExitStatus, Stdio},
};

use anyhow::Result;

use crate::application::{CommandSpec, Exit, Spawned, Spawner, Streams};

/// The `Command` that starts `spec`, its streams left to the caller.
pub fn command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(spec.get_program());
    command.args(spec.get_args());
    for (key, value) in spec.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if let Some(dir) = spec.get_current_dir() {
        command.current_dir(dir);
    }
    if spec.get_new_session() {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
}

/// How the process ended, in the port's terms.
pub fn exit(status: ExitStatus) -> Exit {
    Exit {
        success: status.success(),
        code: status.code(),
        description: status.to_string(),
    }
}

/// Starts processes as children of this one.
pub struct LocalSpawner;

impl Spawner for LocalSpawner {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        let mut command = command(spec);
        match streams {
            Streams::Inherit => {}
            Streams::Null => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            Streams::Files { stdout, stderr } => {
                command
                    .stdin(Stdio::null())
                    .stdout(fs::File::create(stdout)?)
                    .stderr(fs::File::create(stderr)?);
            }
        }
        Ok(Box::new(LocalChild(command.spawn()?)))
    }
}

struct LocalChild(Child);

impl Spawned for LocalChild {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn try_wait(&mut self) -> Result<Option<Exit>> {
        Ok(self.0.try_wait()?.map(exit))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(self.0.kill()?)
    }
    fn wait(&mut self) -> Result<Exit> {
        Ok(exit(self.0.wait()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_starts_with_its_arguments_environment_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (out, err) = (dir.path().join("out"), dir.path().join("err"));
        let mut spec = CommandSpec::new("/bin/sh");
        spec.current_dir(dir.path())
            .env("KEPT", "yes")
            .env("GONE", "no")
            .env_remove("GONE")
            .args(["-c", "printf '%s %s %s' \"$KEPT\" \"${GONE:-unset}\" \"$(basename \"$PWD\")\"; echo oops >&2; exit 3"]);
        let mut child = LocalSpawner
            .spawn(
                &spec,
                Streams::Files {
                    stdout: &out,
                    stderr: &err,
                },
            )
            .unwrap();
        let exit = child.wait().unwrap();
        assert_eq!((exit.success, exit.code), (false, Some(3)));
        assert_eq!(exit.to_string(), "exit status: 3");
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            format!("yes unset {name}")
        );
        assert_eq!(fs::read_to_string(&err).unwrap(), "oops\n");
    }

    #[test]
    fn a_running_child_is_killed_and_reaped() {
        let mut child = LocalSpawner
            .spawn(CommandSpec::new("/bin/sleep").arg("30"), Streams::Null)
            .unwrap();
        assert!(child.id() > 0);
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let exit = child.wait().unwrap();
        assert!(!exit.success && exit.code.is_none(), "{exit}");
        assert!(
            LocalSpawner
                .spawn(&CommandSpec::new("/nonexistent/program"), Streams::Inherit)
                .is_err()
        );
    }

    #[test]
    fn a_new_session_child_leads_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let (out, err) = (dir.path().join("out"), dir.path().join("err"));
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args(["-c", "ps -o sess= -o pgid= -p $$"])
            .new_session();
        let mut child = LocalSpawner
            .spawn(
                &spec,
                Streams::Files {
                    stdout: &out,
                    stderr: &err,
                },
            )
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success);
        // The child is the leader of its process group (setsid(2) made it).
        let fields = fs::read_to_string(&out).unwrap();
        let pgid = fields.split_whitespace().last().unwrap();
        assert_eq!(pgid, pid.to_string(), "{fields}");
    }
}
