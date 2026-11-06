use super::*;
use crate::provision::Ssh;

fn exec_at(url: &str) -> Exec {
    Exec::new(url, "ibk_test", "m_agq")
}

#[test]
fn a_worker_path_never_climbs_out_of_its_root() {
    let e = exec_at("http://127.0.0.1:1");
    assert_eq!(e.abs("models").unwrap(), "/data/home/bm-worker/models");
    assert_eq!(e.abs("").unwrap(), "/data/home/bm-worker");
    assert!(e.abs("/etc/passwd").is_err(), "an absolute path is refused");
    assert!(
        e.abs("models/../../etc").is_err(),
        "a climb is refused even in the middle"
    );
}

#[test]
fn one_shell_word_quotes_its_own_quotes() {
    assert_eq!(shq("/data/home/x"), "'/data/home/x'");
    assert_eq!(shq("it's"), r#"'it'\''s'"#);
    assert_eq!(shq("a b; rm -rf /"), "'a b; rm -rf /'");
}

#[test]
fn a_query_value_escapes_what_a_path_may_hold() {
    assert_eq!(pct("/data/home/a b"), "/data/home/a%20b");
    assert_eq!(pct("/d/ư"), "/d/%C6%B0");
}

#[test]
fn a_run_over_exec_never_falls_back_to_ssh() {
    // The api is a closed port: the call must fail there, naming it, rather
    // than reaching for an ssh binary against a machine that has none.
    let ssh = Ssh {
        target: "m_agq".into(),
        port: 0,
        key: None,
        local: false,
        exec: Some(exec_at("http://127.0.0.1:1")),
    };
    let err = ssh.run("true", 5).expect_err("nothing answers there");
    assert!(err.to_string().contains("127.0.0.1:1"), "{err}");
}

#[test]
fn a_single_file_is_put_not_tarred() {
    // `install_agent` pushes a file, and `tar -C <file>` cannot chdir: the
    // file must reach the files API without a tar in between.
    let dir = std::env::temp_dir().join(format!("bm-exec-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("bm-agent");
    std::fs::write(&file, b"bin").unwrap();
    let e = exec_at("http://127.0.0.1:1");
    let err = e
        .push_dir(&file, "bm-agent", false, &[])
        .expect_err("nothing answers there");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(err.to_string().contains("127.0.0.1:1"), "{err}");
}

#[test]
fn the_api_floor_is_what_an_exec_may_ask_for() {
    // Longer than the API's own ceiling is started detached instead, so the
    // value sent must stay inside it.
    assert_eq!(MAX_EXEC_SECS, 600);
    assert_eq!(
        Exec::new("http://x", "k", "m").root(),
        "/data/home/bm-worker"
    );
}
