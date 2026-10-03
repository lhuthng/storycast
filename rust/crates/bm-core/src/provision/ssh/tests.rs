use super::*;

#[test]
fn progress_parser_reads_the_last_update_and_its_file() {
    // Real `--progress` bytes (openrsync, piped: `\r` between updates,
    // and only the final line per file carries `(xfer#…)`). The last
    // update wins; the file is the last non-progress line.
    let snap = "weights.bin\r         262144  12%  255.37KB/s   00:00:07\r         655360  31%  192.06KB/s   00:00:07\r        2097152 100%  204.68KB/s   00:00:10 (xfer#1, to-check=0/1)\n";
    assert_eq!(
        parse_progress(snap),
        Some(("weights.bin".into(), 2097152, 100, "204.68KB/s".into()))
    );
    // Multi-file: percent resets per file, and the bare intermediate
    // updates (no xfer suffix) must parse as updates, never as filenames.
    let snap2 = "a.bin\r          100 100%  1.00MB/s   00:00:00 (xfer#1, to-check=1/2)\nb.bin\r           50  25%  1.00MB/s   00:00:01\r";
    assert_eq!(
        parse_progress(snap2),
        Some(("b.bin".into(), 50, 25, "1.00MB/s".into()))
    );
    assert_eq!(parse_progress(""), None);
    assert_eq!(parse_progress("sending incremental file list\n"), None);
}

#[test]
fn rsync_watch_streams_progress_while_the_child_runs() {
    // A fake slow transfer through the real runner: progress-shaped
    // stdout for ~1.5 s. First update emits immediately; same-band
    // repeats inside the 2 s window stay silent.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tracker = ProgressTracker::new(tx, "t@h".into(), "models".into());
    let mut cmd = std::process::Command::new("sh");
    cmd.args([
        "-c",
        "echo big.bin; for i in 1 2 3 4 5 6; do echo '  100 10% 1.00MB/s 00:00:01'; sleep 0.25; done",
    ]);
    let watch: OutputWatch<'_> = Some(&mut |out: &str, err: &str| tracker.on_output(out, err));
    let (code, _, _) = run_bounded_live(&mut cmd, 30, "fake push", watch).unwrap();
    assert_eq!(code, 0);
    let line = rx.try_recv().expect("first update streams immediately");
    assert_eq!(line, "[t@h] models: big.bin 10% (0 MB) @ 1.00MB/s");
    assert!(
        rx.try_recv().is_err(),
        "same-band repeats inside 2 s stay silent"
    );
}

#[test]
fn ssh_run_honours_its_timeout_instead_of_blocking_forever() {
    // The swap-voice hang: a never-exiting remote launch wedged the TUI's
    // serial job queue because `run` ignored `timeout_secs`. `sleep` stands
    // in for the wedged command; the local path runs the same wait loop.
    let ssh = Ssh {
        target: "local".into(),
        port: 22,
        key: None,
        local: true,
    };
    let t = std::time::Instant::now();
    let err = ssh.run("exec sleep 30", 1).unwrap_err();
    assert!(
        t.elapsed() < std::time::Duration::from_secs(10),
        "must die near the deadline, not after the sleep"
    );
    assert!(err.to_string().contains("timed out after 1s"), "got: {err}");

    let (code, out, _) = ssh.run("echo hi", 10).expect("a fast command still runs");
    assert_eq!((code, out.trim()), (0, "hi"));
}

#[test]
fn ssh_run_captures_large_stdout_and_stderr() {
    let ssh = Ssh {
        target: "local".into(),
        port: 22,
        key: None,
        local: true,
    };
    let (code, out, err) = ssh.run(
        "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; { dd if=/dev/zero bs=1048576 count=2 2>/dev/null; } >&2; exit 7",
        5,
    ).unwrap();
    assert_eq!(code, 7);
    assert_eq!(out.as_bytes(), vec![0; 2 * 1048576]);
    assert_eq!(err.as_bytes(), vec![0; 2 * 1048576]);
}

#[test]
fn ssh_run_does_not_wait_for_detached_output_handles() {
    let ssh = Ssh {
        target: "local".into(),
        port: 22,
        key: None,
        local: true,
    };
    let start = std::time::Instant::now();
    let result = ssh
        .run("sleep 5 & echo started; echo warning >&2; exit 7", 1)
        .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(result, (7, "started\n".into(), "warning\n".into()));
}

#[test]
fn bounded_runner_preserves_transport_context() {
    let err = run_bounded(
        Command::new("sh").args(["-c", "exec sleep 30"]),
        1,
        "rsync pull from user@host",
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("rsync pull from user@host timed out after 1s"));
    let err = run_bounded(
        &mut Command::new("/nonexistent/bm-rsync"),
        1,
        "rsync push to user@host",
    )
    .unwrap_err();
    assert!(err.to_string().contains("spawning rsync push to user@host"));
}

#[test]
fn ssh_argv_expands_tilde_in_the_key_for_both_transports() {
    let _env = crate::ENV_LOCK.lock().unwrap();
    // The ledger held `~/.ssh/ssh-key-my-wsl` verbatim; ssh (no shell)
    // failed it while rsync (shell) expanded it. Both now go through
    // expand_tilde, so `-i` always names a real path.
    let home = std::env::var("HOME").unwrap();
    let ssh = Ssh {
        target: "thang@192.168.2.2".into(),
        port: 22,
        key: Some("~/.ssh/k".into()),
        local: false,
    };
    let args = ssh.ssh_args();
    let i = args
        .iter()
        .position(|a| a == "-i")
        .expect("key flag present");
    assert_eq!(args[i + 1], format!("{home}/.ssh/k"), "ssh argv: {args:?}");
    assert_eq!(
        ssh.rsync_e(),
        format!(
            "ssh -o BatchMode=yes -o ConnectTimeout=10 {} -p 22 -i {home}/.ssh/k",
            HOST_KEY_OPTS.join(" ")
        )
    );

    let bare = Ssh {
        target: "t@h".into(),
        port: 2222,
        key: None,
        local: false,
    };
    assert!(
        !bare.ssh_args().contains(&"-i".to_string()),
        "no key, no flag"
    );
    assert!(!bare.rsync_e().contains("-i"), "no key, no flag");
}

#[test]
fn resolve_key_prefers_box_then_settings_then_ssh_default() {
    let _env = crate::ENV_LOCK.lock().unwrap();
    let home = std::env::var("HOME").unwrap();
    let (p, src) = resolve_key(Some("~/.ssh/box-k"), Some("~/.ssh/app-k"));
    assert_eq!(
        (p.unwrap(), src),
        (PathBuf::from(format!("{home}/.ssh/box-k")), KeySource::Box)
    );
    let (p, src) = resolve_key(None, Some("/k/app"));
    assert_eq!(
        (p.unwrap(), src),
        (PathBuf::from("/k/app"), KeySource::Settings)
    );
    // Empty strings fall through: clearing the field unsets the key.
    let (p, src) = resolve_key(Some("  "), Some(""));
    assert_eq!((p, src), (None, KeySource::SshDefault));
    let (p, src) = resolve_key(None, None);
    assert_eq!((p, src), (None, KeySource::SshDefault));
    assert_eq!(KeySource::Box.label(), "machines.json");
}

#[test]
fn ssh_never_prompts_never_lingers() {
    // Every ssh use is scripted: no stdin, no password prompts, and a
    // stalled connection must die instead of hanging a TUI job forever.
    let args = Ssh::for_machine(&Machine::new("192.168.2.2", "thang", 22, None, "worker"))
        .ssh_args()
        .join(" ");
    for flag in [
        "-n",
        "BatchMode=yes",
        "ConnectTimeout=10",
        "ServerAliveInterval=5",
        "ServerAliveCountMax=2",
    ] {
        assert!(args.contains(flag), "{args}");
    }
}

#[test]
fn both_transports_decline_host_key_verification() {
    // The bug this pins: a freshly launched EC2 instance presents a host key
    // nobody has seen, and `BatchMode=yes` forbids the prompt, so every
    // first provision died with `exit 255: Host key verification failed`.
    // The fix has to be on *both* transports — rsync spawns its own ssh, so
    // a policy on the direct one alone would fix `probe` and leave every
    // push failing identically.
    let ssh = Ssh::for_machine(&Machine::new("3.121.112.113", "ubuntu", 22, None, "worker"));

    let args = ssh.ssh_args().join(" ");
    assert!(args.contains("StrictHostKeyChecking=no"), "{args}");
    assert!(args.contains("UserKnownHostsFile=/dev/null"), "{args}");
    // `~/.ssh/config` must still be read: an -o overrides one option, it
    // does not replace the file. `-F /dev/null` would silently drop Host
    // aliases, ProxyJump and IdentityFile.
    assert!(!args.contains("-F /dev/null"), "{args}");
    assert!(!args.contains("-F/dev/null"), "{args}");

    let e = ssh.rsync_e();
    assert!(e.contains("StrictHostKeyChecking=no"), "{e}");
    assert!(e.contains("UserKnownHostsFile=/dev/null"), "{e}");
    assert!(!e.contains("-F /dev/null"), "{e}");

    // A local machine never goes through either: it runs `sh` in place.
    let local = Ssh::for_machine(&Machine::new("127.0.0.1", "me", 22, None, "worker"));
    assert!(local.local, "the local node must not be ssh'd to");
}

#[test]
fn localhost_is_detected_as_local() {
    for addr in ["127.0.0.1", "localhost", "::1"] {
        let m = Machine::new(addr, "me", 22, None, "worker");
        assert!(Ssh::for_machine(&m).local, "{addr} should be local");
    }
    let remote = Machine::new("192.168.2.2", "thang", 22, None, "worker");
    assert!(!Ssh::for_machine(&remote).local);
}

#[test]
fn copy_dir_skips_venv_and_caches() {
    let src = std::env::temp_dir().join("bm-provision-src");
    let dst = std::env::temp_dir().join("bm-provision-dst");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(src.join(".venv")).unwrap();
    std::fs::create_dir_all(src.join("__pycache__")).unwrap();
    std::fs::write(src.join("tts_server.py"), "x").unwrap();
    std::fs::write(src.join(".venv/marker"), "x").unwrap();
    copy_dir(&src, &dst).unwrap();
    assert!(dst.join("tts_server.py").exists());
    assert!(!dst.join(".venv").exists(), "venv must never be copied");
    assert!(!dst.join("__pycache__").exists());
}

#[test]
fn ssh_local_agrees_with_is_local_node() {
    // One predicate, one place: the provisioner's `Ssh.local` and the
    // offer's `local_node` flag must never disagree.
    for addr in ["127.0.0.1", "localhost", "::1", "192.168.2.2", "10.0.0.5"] {
        let m = Machine::new(addr, "u", 22, None, "worker");
        assert_eq!(
            Ssh::for_machine(&m).local,
            crate::is_local_node(addr),
            "{addr}"
        );
    }
}

#[test]
fn ssh_args_include_port_and_key_only_when_set() {
    let m = Machine::new("10.0.0.5", "pi", 2222, Some("/k/id".into()), "worker");
    let ssh = Ssh::for_machine(&m);
    let args = ssh.ssh_args();
    assert!(args.contains(&"-p".to_string()));
    assert!(args.contains(&"2222".to_string()));
    assert!(args.contains(&"/k/id".to_string()));
    assert_eq!(args.last().unwrap(), "pi@10.0.0.5");

    let m2 = Machine::new("10.0.0.6", "pi", 22, None, "worker");
    let args2 = Ssh::for_machine(&m2).ssh_args();
    assert!(!args2.contains(&"-p".to_string()));
    assert!(!args2.contains(&"-i".to_string()));
}

#[test]
fn copy_dir_leaves_an_unchanged_signature_alone() {
    // The local fast path compares size+mtime, exactly like rsync. To prove
    // the skip (a real identical file cannot be told apart anyway), the
    // destination is given different *content* with the same signature: if
    // it is copied over, the comparison did not happen.
    let src = std::env::temp_dir().join("bm-copy-skip-src");
    let dst = std::env::temp_dir().join("bm-copy-skip-dst");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), "same").unwrap();
    copy_dir(&src, &dst).unwrap();

    std::fs::write(dst.join("a.txt"), "diff").unwrap();
    let t = std::fs::metadata(src.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(dst.join("a.txt"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    copy_dir(&src, &dst).unwrap();
    assert_eq!(
        std::fs::read_to_string(dst.join("a.txt")).unwrap(),
        "diff",
        "an identical size+mtime must not be re-copied"
    );

    // A changed size is a real change and must be copied.
    std::fs::write(src.join("a.txt"), "a longer body").unwrap();
    copy_dir(&src, &dst).unwrap();
    assert_eq!(
        std::fs::read_to_string(dst.join("a.txt")).unwrap(),
        "a longer body"
    );
}

#[test]
fn only_blips_retry_never_auth_or_host_key() {
    // The exact strings a flapping link produces — and the two that must
    // fail fast instead of burning four attempts of backoff.
    for (code, stderr) in [
        (255, "ssh: connect to host h port 22: Operation timed out"),
        (255, "ssh: connect to host h port 22: Connection refused"),
        (255, "Connection reset by peer"),
        (255, "rsync: connection unexpectedly closed"),
        (255, "rsync error: timeout waiting for daemon (30)"),
        (10, "rsync error: error in socket IO (code 10)"),
        (255, "client_loop: send disconnect: Broken pipe"),
    ] {
        assert!(transient_failure(code, stderr), "{stderr}");
    }
    for (code, stderr) in [
        (255, "Permission denied (publickey)"),
        (255, "Host key verification failed."),
        (1, "Operation timed out"),
    ] {
        assert!(!transient_failure(code, stderr), "{stderr}");
    }
}

#[test]
fn transport_retries_a_blip_then_returns_success() {
    // One 2 s backoff, then the identical call succeeds — the flap `:prov`
    // used to die on.
    let mut n = 0;
    let (code, _, _) = with_transport_retries(|| {
        n += 1;
        if n < 2 {
            Ok((
                255,
                String::new(),
                "ssh: connect to host h port 22: Operation timed out".into(),
            ))
        } else {
            Ok((0, "ok".into(), String::new()))
        }
    })
    .unwrap();
    assert_eq!((code, n), (0, 2));
}
