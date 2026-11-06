use super::*;

/// A box the registry knows, saved where `reclaim` will have to delete it from.
fn owned_box(inner: &mut Inner, addr: &str, provider: Option<(&str, &str)>) -> Machine {
    let mut m = Machine::new(addr, "ubuntu", 22, None, "worker");
    m.state = MachineState::Online;
    if let Some((p, id)) = provider {
        m.provider = Some(p.into());
        m.provider_id = Some(id.into());
    }
    let (bxo, _rt) = bm_core::provision::split_machine(&m, "box");
    bm_core::provision::save_box(&inner.layout.machines(), &bxo).unwrap();
    inner.machines.insert(addr.into(), m.clone());
    m
}

fn held_by(inner: &mut Inner, worker: &str, addr: &str) -> String {
    inner.workers.insert(worker.into(), addr.into());
    let mut t = Task::new(7, Stage::Digest);
    t.state = TaskState::Assigned;
    t.assigned_to = Some(worker.into());
    t.lease_until = Some(now_secs() + 600);
    let id = t.id();
    inner.tasks.insert(id.clone(), t);
    id
}

#[test]
fn a_killed_box_hands_its_rows_back_and_owes_one_delete() {
    let (_d, mut inner) = fixture();
    let addr = "203.0.113.7";
    owned_box(&mut inner, addr, Some(("aws", "i-0abc")));
    let id = held_by(&mut inner, "w1", addr);

    let line = inner.reclaim(addr, "idle-paused").expect("a line to log");
    assert!(line.contains("killed: idle-paused"), "{line}");
    assert_eq!(inner.tasks[&id].state, TaskState::Pending, "work came back");
    assert_eq!(inner.tasks[&id].attempts, 0, "and strike-free");
    assert!(!inner.machines.contains_key(addr), "the row is gone");
    assert!(
        !bm_core::provision::load_boxes(&inner.layout.machines())
            .iter()
            .any(|b| b.addr == addr),
        "and so is machines.json's"
    );
    assert_eq!(inner.intents.len(), 1, "one delete owed");
    let intent = &inner.intents[0];
    assert_eq!(
        (intent.provider.as_str(), intent.provider_id.as_str()),
        ("aws", "i-0abc")
    );
    assert_eq!(intent.why, "idle-paused");
}

#[test]
fn the_second_verdict_on_one_box_books_no_second_delete() {
    let (_d, mut inner) = fixture();
    let addr = "203.0.113.8";
    owned_box(&mut inner, addr, Some(("aws", "i-0def")));
    assert!(inner.reclaim(addr, "spot reclaim").is_some());
    // A deadline verdict arriving after the box is already out: nothing left.
    assert!(inner.reclaim(addr, "past its deadline").is_none());
    assert_eq!(inner.intents.len(), 1, "one intent per provider id");
}

#[test]
fn a_racing_row_keeps_the_holder_that_is_still_alive() {
    let (_d, mut inner) = fixture();
    let addr = "203.0.113.9";
    owned_box(&mut inner, addr, Some(("aws", "i-0ghi")));
    inner.workers.insert("w1".into(), addr.into());
    inner.workers.insert("w2".into(), "198.51.100.2".into());
    let mut t = Task::new(9, Stage::Digest);
    t.state = TaskState::Running;
    t.assigned_to = Some("w1".into());
    t.racers = vec!["w2".into()];
    t.lease_until = Some(now_secs() + 600);
    inner.tasks.insert(t.id(), t);

    inner.reclaim(addr, "provider 404");
    let row = &inner.tasks["digest:9"];
    assert!(!row.state.is_terminal(), "still racing, not requeued");
    assert_eq!(
        row.holders(),
        vec!["w2".to_string()],
        "only w1's share went"
    );
    assert!(row.detail.contains("still racing"), "{}", row.detail);
}

#[test]
fn a_box_with_no_provider_id_is_marked_killed_and_kept() {
    let (_d, mut inner) = fixture();
    let addr = "192.168.2.50";
    owned_box(&mut inner, addr, None);

    let line = inner.reclaim(addr, "past its deadline").expect("a line");
    assert!(line.contains("no provider id"), "{line}");
    let m = inner.machines.get(addr).expect("kept for the operator");
    assert_eq!(m.state, MachineState::Killed);
    assert!(m.note.contains("killed: past its deadline"), "{}", m.note);
    assert!(!m.state.accepts_work(), "and it is never offered work");
    assert!(inner.intents.is_empty(), "nothing was queued for deletion");
}

#[test]
fn a_delete_stops_being_retried_and_a_reap_starts_it_again() {
    let (_d, mut inner) = fixture();
    owned_box(&mut inner, "203.0.113.10", Some(("aws", "i-0jkl")));
    inner.reclaim("203.0.113.10", "stopped");

    for n in 1..=crate::state::reclaim::MAX_DELETE_ATTEMPTS {
        let next = inner.next_intent().expect("still under the cap");
        assert_eq!(next.attempts, n - 1);
        let line = inner
            .settle_intent("i-0jkl", Err("UnauthorizedOperation".into()))
            .expect("a line");
        assert!(line.contains(&format!("failed ({n})")), "{line}");
        assert_eq!(inner.intents_pending(), 1, "a failure keeps the row");
    }
    assert!(
        inner.next_intent().is_none(),
        "past the cap the sweep leaves it for the operator"
    );
    let line = inner.retry_intents();
    assert!(line.contains("retried"), "{line}");
    assert!(
        inner.next_intent().is_some(),
        "and a reap gives it another go"
    );
    assert_eq!(inner.intents.len(), 1, "retrying does not duplicate it");
}

#[test]
fn a_settled_delete_is_forgotten() {
    let (_d, mut inner) = fixture();
    owned_box(&mut inner, "203.0.113.11", Some(("aws", "i-0mno")));
    inner.reclaim("203.0.113.11", "terminated");
    let line = inner.settle_intent("i-0mno", Ok(())).expect("a line");
    assert!(line.contains("is gone"), "{line}");
    assert_eq!(inner.intents_pending(), 0);
    assert!(
        inner.settle_intent("i-0mno", Ok(())).is_none(),
        "idempotent"
    );
}

#[test]
fn the_queue_survives_a_restart() {
    let (_d, mut inner) = fixture();
    owned_box(&mut inner, "203.0.113.12", Some(("aws", "i-0pqr")));
    inner.reclaim("203.0.113.12", "max-run");

    // A fresh process reads the same ledger back.
    let layout = inner.layout.clone();
    let mut again = Inner::distributing(layout, Settings::default());
    again.load_ledger();
    assert_eq!(again.intents_pending(), 1);
    assert_eq!(again.intents[0].provider_id, "i-0pqr");
    assert_eq!(again.intents[0].why, "max-run");
}
