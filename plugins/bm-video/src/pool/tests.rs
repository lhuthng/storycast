use super::*;

fn at(age: Duration) -> Instant {
    // `Instant` cannot be constructed backwards, so borrow one and step it back.
    Instant::now() - age
}

#[test]
fn batches_are_contiguous_and_stop_at_the_end() {
    let mut p = Pool { n: 10, ..Default::default() };
    assert_eq!(p.take(4), Some((0, 4)));
    assert_eq!(p.take(4), Some((4, 4)));
    assert_eq!(p.take(4), Some((8, 2))); // clipped to what is left
    assert_eq!(p.take(4), None);
    // Nothing left to hand out, but three batches are still out there with
    // boxes that may yet come back.
    assert!(!p.unassigned());
    assert_eq!(p.held.len(), 3);
}

#[test]
fn a_batch_nobody_finishes_goes_back_in_the_pool() {
    // A box that dies holding a lease must not strand its chunks: the fallback
    // is that the plan owner renders them itself, which is wasted work.
    let mut p = Pool { n: 8, ..Default::default() };
    assert_eq!(p.take(4), Some((0, 4)));
    p.held = vec![(0, 4, at(LEASE + Duration::from_secs(1)))];
    assert_eq!(p.take(4), Some((0, 4)));
}

#[test]
fn an_expired_lease_hands_back_only_what_is_still_missing() {
    // A box submits, the server never hears the batch close, and the lease runs
    // out: re-handing the whole batch makes a second box redo finished work.
    let mut p = Pool { n: 8, ..Default::default() };
    assert_eq!(p.take(4), Some((0, 4)));
    p.submitted("00000000-aaaa.mp4", 120);
    p.submitted("00000120-bbbb.mp4", 120);
    p.held = vec![(0, 4, at(LEASE + Duration::from_secs(1)))];
    assert_eq!(p.take(4), Some((2, 2)));
}

#[test]
fn a_finished_lease_is_not_handed_back_at_all() {
    let mut p = Pool { n: 4, ..Default::default() };
    assert_eq!(p.take(4), Some((0, 4)));
    for i in 0..4 {
        p.submitted(&format!("{:08}-key.mp4", i * 120), 120);
    }
    p.held = vec![(0, 4, at(LEASE + Duration::from_secs(1)))];
    assert_eq!(p.take(4), None);
    assert!(p.finished());
}

#[test]
fn a_name_says_which_chunk_it_is() {
    let mut p = Pool { n: 40, ..Default::default() };
    p.submitted("00001200-deadbeef.mp4", 120);
    p.submitted("not-a-chunk.mp4", 120);
    assert!(p.received.contains(&10));
    assert_eq!(p.received.len(), 1);
}

#[test]
fn a_held_batch_is_not_handed_out_twice() {
    let mut p = Pool { n: 8, ..Default::default() };
    assert_eq!(p.take(4), Some((0, 4)));
    assert_eq!(p.take(4), Some((4, 4)));
    assert_eq!(p.take(4), None);
}

#[test]
fn receiving_every_chunk_finishes_the_pool() {
    let mut p = Pool { n: 2, ..Default::default() };
    assert!(!p.finished());
    p.done.insert("a.mp4".into());
    assert!(!p.finished());
    p.done.insert("b.mp4".into());
    assert!(p.finished());
}

#[test]
fn a_path_that_climbs_out_of_the_root_is_refused() {
    let root = Path::new("/tmp/pool-root");
    assert!(safe_join(root, "tmp/a.png").is_ok());
    assert!(safe_join(root, "../etc/passwd").is_err());
    assert!(safe_join(root, "a/../../b").is_err());
    assert!(safe_join(root, "/etc/passwd").is_err());
    assert!(safe_join(root, "").is_err());
}

#[test]
fn a_line_and_a_body_survive_the_wire_together() {
    // The framing is the one piece both ends must agree on exactly: a line of
    // JSON, then a body of the length it announced.
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    let echo = std::thread::spawn(move || -> Result<()> {
        let (s, _) = l.accept()?;
        let mut c = Conn::new(s)?;
        let req: Ask = serde_json::from_value(c.line()?)?;
        assert_eq!(req.op, "submit");
        let body = c.body(req.len)?;
        assert_eq!(body, b"hello");
        c.send(&Ack { ok: true, len: body.len(), ..Default::default() })?;
        c.send_raw(b"bye")?;
        // A second line straight after a body must still frame correctly.
        c.send(&Ack { ok: true, from: 7, ..Default::default() })?;
        Ok(())
    });

    let mut c = connect(&addr.to_string()).expect("connect");
    c.send(&Ask { op: "submit".into(), name: "x".into(), len: 5, ..blank() })
        .expect("send");
    c.send_raw(b"hello").expect("body");
    let ack: Ack = serde_json::from_value(c.line().expect("ack")).expect("parse");
    assert_eq!(ack.len, 5);
    assert_eq!(c.body(3).expect("raw"), b"bye");
    let second: Ack = serde_json::from_value(c.line().expect("second")).expect("parse");
    assert_eq!(second.from, 7);

    echo.join().expect("the server thread panicked").expect("the server errored");
}

#[test]
fn a_batch_is_computed_from_the_chunk_clock() {
    // The worker finds its chunks by frame offset, so this has to line up with
    // how the plan names them.
    let dir = std::env::temp_dir().join(format!("bm-video-pool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join("00000240-abcdef.mp4"), b"x").expect("write");
    std::fs::write(dir.join("00000360-123456.mp4"), b"x").expect("write");
    let found = find_chunk(&dir, 240).expect("find");
    assert_eq!(found.map(|(n, _)| n).as_deref(), Some("00000240-abcdef.mp4"));
    assert!(find_chunk(&dir, 480).expect("find").is_none());
    std::fs::remove_dir_all(&dir).ok();
}
