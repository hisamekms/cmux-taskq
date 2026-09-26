//! The real cmux adapter against stub `cmux` executables: the detached
//! environment and ping, `notify`, one-line sends and the resume workspace,
//! and the marks and unpin before a close.

use dagq::{
    application::{DetachedRefusal, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags},
    domain::{Task, TaskRun},
    infrastructure::adapters::{Cmux, SOCKET_PASSWORD_ENV, detach, process_alive},
};
use std::{ffi::OsString, fs, process::Command, thread, time::Duration};

/// The detached environment keeps nothing of the cmux session `up` runs
/// in: every inherited `CMUX_*` variable is removed (the socket capability
/// and the workspace, surface and socket paths among them), PATH is the
/// agent's, and the password is the exported one or absent.
#[test]
fn detached_command_drops_every_inherited_cmux_variable_but_the_password() {
    let inherited = [
        "CMUX_SOCKET_CAPABILITY",
        "CMUX_SOCKET_PATH",
        "CMUX_WORKSPACE_ID",
        "CMUX_SURFACE_ID",
        "CMUX_SOCKET_PASSWORD",
        "HOME",
        "PATH",
        "NOT_CMUX_",
    ]
    .map(OsString::from);
    let environment = SupervisorEnvironment {
        path: "/agent/bin".into(),
        socket_password: None,
    };
    let mut command = Command::new("cmux");
    detach(&mut command, inherited.clone(), &environment);
    let envs: Vec<(String, Option<String>)> = command
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs,
        vec![
            ("CMUX_SOCKET_CAPABILITY".to_owned(), None),
            ("CMUX_SOCKET_PASSWORD".to_owned(), None),
            ("CMUX_SOCKET_PATH".to_owned(), None),
            ("CMUX_SURFACE_ID".to_owned(), None),
            ("CMUX_WORKSPACE_ID".to_owned(), None),
            ("PATH".to_owned(), Some("/agent/bin".to_owned())),
        ]
    );

    let mut command = Command::new("cmux");
    detach(
        &mut command,
        inherited,
        &SupervisorEnvironment {
            path: "/agent/bin".into(),
            socket_password: Some("pw".into()),
        },
    );
    let password = command
        .get_envs()
        .find(|(name, _)| *name == SOCKET_PASSWORD_ENV)
        .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    assert_eq!(password, Some(Some("pw".to_owned())));
    assert_eq!(
        command
            .get_envs()
            .filter(|(name, value)| name.to_string_lossy().starts_with("CMUX_") && value.is_some())
            .count(),
        1
    );
}

/// The real adapter's `notify` is `cmux notify --title … --body …`, with
/// `--workspace` only when a target is given; a failing cmux is an error.
#[test]
fn the_cmux_adapter_notifies_with_title_body_and_an_optional_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n[ \"$1\" = notify ] || exit 2\n[ \"$3\" != fail ]\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    let args = || fs::read_to_string(&dump).unwrap();
    cmux.notify("dagq repo: task 1 failed", "a task\nnext: x", Some("WS"))
        .unwrap();
    assert_eq!(
        args(),
        "notify\n--title\ndagq repo: task 1 failed\n--body\na task\nnext: x\n--workspace\nWS\n"
    );
    cmux.notify("title", "body", None).unwrap();
    assert_eq!(args(), "notify\n--title\ntitle\n--body\nbody\n");
    assert!(cmux.notify("fail", "body", None).is_err());
}

/// The real adapter types a resolution request as one line (line breaks
/// and tabs, which `cmux send` would read as keys, become spaces, and
/// backslashes slashes) and submits it with Enter, and opens a resume
/// workspace in the run's worktree named like the run's worker workspace
/// (`[<repo>]worker#<task-id> - <task title>`, ADR-0028).
#[test]
fn the_cmux_adapter_sends_one_line_and_names_the_resume_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$1\" in workspace) echo 'OK workspace:7' ;; --json) echo '{{\"caller\":{{\"workspace_id\":\"01234567-89ab-4def-8123-000000000007\"}}}}' ;; esac\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    cmux.send_text("WS", "line one\n\tline two\\n\n").unwrap();
    assert_eq!(
        fs::read_to_string(&dump).unwrap(),
        "send\n--workspace\nWS\n--\nline one line two/n\nsend-key\n--workspace\nWS\n--\nenter\n"
    );
    fs::remove_file(&dump).unwrap();
    let run = TaskRun::restore(dagq::domain::RunRecord {
        id: dagq::domain::RunId::new("run-1").unwrap(),
        task_id: dagq::domain::TaskId::new(3),
        status: dagq::domain::RunStatus::NeedsSession,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        base_commit: dagq::domain::CommitSha::try_from("0".repeat(40)).unwrap(),
        repo_path: Some("/src/my-repo".into()),
        run_dir: Some(dir.path().to_string_lossy().into_owned()),
        worktree_path: Some(dir.path().to_string_lossy().into_owned()),
        branch: None,
        workspace_id: None,
        receipt_path: None,
        log_path: None,
        result_commit: None,
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap();
    assert_eq!(
        cmux.create_resume(
            &Task::restore(dagq::domain::TaskRecord {
                id: dagq::domain::TaskId::new(3),
                title: "fix it".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                status: dagq::domain::TaskStatus::InProgress,
                goal_id: None,
                context: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap(),
            &run,
            "runner session --resume",
            &WorkspaceTags {
                env: vec![("DAGQ_ROLE".into(), "worker".into())],
                description: Some(
                    dagq::infrastructure::adapters::resume_workspace_description(&run)
                ),
                group: Some("group:1".into()),
            },
        )
        .unwrap(),
        "01234567-89ab-4def-8123-000000000007"
    );
    let args = fs::read_to_string(&dump).unwrap();
    assert!(
        args.starts_with(&format!(
            "workspace\ncreate\n--name\n[my-repo]worker#3 - fix it\n--description\nrun run-1 resume\n--env\nDAGQ_ROLE=worker\n--group\ngroup:1\n--command\nrunner session --resume\n--focus\nfalse\n--cwd\n{}\n",
            dir.path().display()
        )),
        "{args}"
    );
}

/// The real adapter runs `cmux ping` in that environment and outside
/// cmux's process tree: a stub cmux dumps what it was given and who its
/// parent is, and this test process (which may itself run inside cmux)
/// leaks none of its `CMUX_*` variables into it, while launchd (pid 1) is
/// the stub's parent, as it is the LaunchAgent supervisor's.
#[test]
fn the_cmux_adapter_pings_orphaned_with_the_detached_environment() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("env.txt");
    let stub = dir.path().join("cmux-stub");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n[ \"$1\" = ping ] || exit 2\n/usr/bin/env > '{0}'\necho \"PARENT=$PPID\" >> '{0}'\nprintf 'PONG\\n'\n",
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux {
        executable: stub.clone(),
    };
    cmux.preflight_detached(&SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: Some("pw".into()),
    })
    .unwrap();
    let seen: Vec<(String, String)> = fs::read_to_string(&dump)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let cmux_variables: Vec<&(String, String)> = seen
        .iter()
        .filter(|(name, _)| name.starts_with("CMUX_"))
        .collect();
    assert_eq!(
        cmux_variables,
        vec![&("CMUX_SOCKET_PASSWORD".to_owned(), "pw".to_owned())],
        "{seen:?}"
    );
    assert!(seen.contains(&("PATH".to_owned(), "/usr/bin:/bin".to_owned())));
    assert!(
        seen.contains(&("PARENT".to_owned(), "1".to_owned())),
        "{seen:?}"
    );

    // Without an exported password none reaches the stub either.
    cmux.preflight_detached(&SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: None,
    })
    .unwrap();
    assert!(
        !fs::read_to_string(&dump)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("CMUX_"))
    );

    // A refusal is cmux's stderr; a wrong reply is reported as such.
    let environment = SupervisorEnvironment {
        path: "/usr/bin:/bin".into(),
        socket_password: None,
    };
    fs::write(
        &stub,
        "#!/bin/sh\necho 'only processes started inside cmux can connect' >&2\nexit 1\n",
    )
    .unwrap();
    let error = cmux.preflight_detached(&environment).unwrap_err();
    assert!(error.is::<DetachedRefusal>(), "{error:#}");
    let error = format!("{error:#}");
    assert!(error.contains("ping from outside cmux failed"), "{error}");
    assert!(
        error.ends_with("only processes started inside cmux can connect"),
        "{error}"
    );
    fs::write(&stub, "#!/bin/sh\necho PING\n").unwrap();
    let error = cmux.preflight_detached(&environment).unwrap_err();
    assert!(error.is::<DetachedRefusal>(), "{error:#}");
    assert!(
        format!("{error:#}").ends_with("unexpected response: PING"),
        "{error:#}"
    );

    // A ping that hangs is killed at the deadline and reported.
    let pid_file = dir.path().join("pid.txt");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let error = cmux
        .preflight_detached_within(&environment, Duration::from_millis(300))
        .unwrap_err();
    assert!(!error.is::<DetachedRefusal>(), "{error:#}");
    assert!(
        format!("{error:#}").ends_with("did not finish within 300ms"),
        "{error:#}"
    );
    let pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!process_alive(pid), "sleep {pid} outlived the deadline");
}

/// The real adapter's look calls are `workspace-action --action set-color
/// --color <c>`, `set-status <key> <value> --icon <i>` and
/// `workspace-action --action pin`, each with `--workspace <uuid>`; `close`
/// unpins before it closes, since cmux refuses to close a pinned
/// workspace, and an unpin that fails does not stop the close.
#[test]
fn the_cmux_adapter_marks_a_workspace_and_unpins_it_before_closing() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    // The unpin of `GONE` and the close of `STUCK` fail.
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" >> '{}'\necho >> '{}'\ncase \"$*\" in *'unpin --workspace GONE') exit 1 ;; 'workspace close STUCK') exit 1 ;; 'workspace close'*) echo \"OK workspace:3\" ;; *) echo OK ;; esac\n",
            dump.display(),
            dump.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let cmux = Cmux { executable: stub };
    let calls = || {
        let calls = fs::read_to_string(&dump).unwrap_or_default();
        let _ = fs::remove_file(&dump);
        calls
    };
    cmux.set_color("WS", "Amber").unwrap();
    cmux.set_status("WS", "dagq_role", "inbox", "tray").unwrap();
    cmux.pin("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action set-color --color Amber --workspace WS \n\
set-status dagq_role inbox --icon tray --workspace WS \n\
workspace-action --action pin --workspace WS \n"
    );
    cmux.close("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace WS \nworkspace close WS \n"
    );
    cmux.close("GONE").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace GONE \nworkspace close GONE \n"
    );
    assert!(cmux.close("STUCK").is_err());
    assert!(cmux.set_color("GONE", "Blue").is_ok());
}
