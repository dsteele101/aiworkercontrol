//! End-to-end lifecycle of items through the public queue API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::fs;
use std::sync::Arc;
use std::thread;

use work_queue::{Queue, State, WorkItem};

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn item_moves_open_to_inprogress_to_closed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "open/phase/timesifter.txt", "No extra turns");
    let queue = Queue::new(dir.path());

    let item = queue.claim_next(|_| false).unwrap().unwrap();
    assert_eq!(item, WorkItem::new("phase", "timesifter.txt"));
    assert!(queue.list(State::Open).unwrap().is_empty());
    assert_eq!(queue.list(State::InProgress).unwrap(), vec![item.clone()]);

    queue.complete(&item, "done").unwrap();
    assert_eq!(queue.list(State::Closed).unwrap(), vec![item.clone()]);
    assert_eq!(queue.read(State::Closed, &item).unwrap(), "No extra turns");
    assert!(queue.claim_next(|_| false).unwrap().is_none());
}

#[test]
fn released_item_can_be_claimed_again() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "open/p/a.txt", "");
    let queue = Queue::new(dir.path());

    let item = queue.claim_next(|_| false).unwrap().unwrap();
    queue.release(&item).unwrap();
    assert_eq!(queue.claim_next(|_| false).unwrap(), Some(item));
}

#[test]
fn recover_orphans_reopens_inprogress_items() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inprogress/p/a.txt", "");
    write(dir.path(), "inprogress/q/b.txt", "");
    let queue = Queue::new(dir.path());

    let recovered = queue.recover_orphans().unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(queue.list(State::Open).unwrap(), recovered);
    assert!(queue.list(State::InProgress).unwrap().is_empty());
}

#[test]
fn concurrent_claims_never_share_an_item() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..20 {
        write(dir.path(), &format!("open/p/{i:02}.txt"), "");
    }
    let queue = Arc::new(Queue::new(dir.path()));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                let mut mine = Vec::new();
                while let Some(item) = queue.claim_next(|_| false).unwrap() {
                    mine.push(item);
                }
                mine
            })
        })
        .collect();

    let mut all: Vec<WorkItem> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    all.sort();
    let before = all.len();
    all.dedup();
    assert_eq!(before, 20);
    assert_eq!(all.len(), 20);
}

#[test]
fn sidecars_move_with_their_item_through_every_state() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inprogress/p/a.txt", "body");
    write(dir.path(), "inprogress/p/a.meta.json", "{}");
    let queue = Queue::new(dir.path());
    let item = WorkItem::new("p", "a.txt");
    queue
        .write_result(State::InProgress, &item, "report")
        .unwrap();

    let path = [
        State::InProgress,
        State::InReview,
        State::Hold,
        State::Open,
        State::Closed,
    ];
    for pair in path.windows(2) {
        queue.transition(&item, pair[0], pair[1]).unwrap();
        assert_eq!(
            queue.list(pair[1]).unwrap(),
            vec![item.clone()],
            "{:?}",
            pair[1]
        );
        assert!(queue.list(pair[0]).unwrap().is_empty());
    }
    let closed = dir.path().join("closed/p");
    assert_eq!(
        fs::read_to_string(closed.join("a.result.md")).unwrap(),
        "report"
    );
    assert_eq!(
        fs::read_to_string(closed.join("a.meta.json")).unwrap(),
        "{}"
    );
    assert!(!dir.path().join("open/p/a.meta.json").exists());
}

#[test]
fn take_claims_from_any_state_once() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inreview/p/a.txt", "");
    let queue = Queue::new(dir.path());
    let item = WorkItem::new("p", "a.txt");

    assert!(queue.take(&item, State::InReview).unwrap());
    assert!(
        !queue.take(&item, State::InReview).unwrap(),
        "already taken"
    );
    assert_eq!(queue.list(State::InProgress).unwrap(), vec![item]);
}

#[test]
fn every_state_has_its_own_directory() {
    let names: Vec<_> = State::ALL.iter().map(|s| s.dir_name()).collect();
    assert_eq!(names, ["open", "inprogress", "inreview", "closed", "hold"]);
}
