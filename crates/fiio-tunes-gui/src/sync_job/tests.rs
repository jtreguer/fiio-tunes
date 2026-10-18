//! The sync worker: its pure parts, and `plan` and `apply` against a card
//! that is a temp folder (`device.mountpoint` with `allow_folder`). The
//! library, the playlists folder, the index and the data folder are temp
//! dirs too; the fixtures are the core crate's integration-test helpers.
//! `/proc/self/mountinfo` is read, as the workers do, to describe the card
//! folder: it is not a mount, so it has no source and no USB device.

// The worker tests each include the fixtures: one copy per test module.
#[allow(clippy::duplicate_mod)]
#[path = "../../../fiio-core/tests/common/mod.rs"]
mod common;
#[allow(clippy::duplicate_mod)]
#[path = "../test_world.rs"]
mod world;

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use fiio_core::plan::{CopyOp, MoveOp, ReplaceOp, ReplaceReason};
use fiio_core::store;
use world::{World, tree};

use super::*;

fn copying() -> Progress {
    Progress::Copying {
        done: 0,
        total: 2,
        bytes: 0,
        bytes_total: 10,
        name: "a.mp3".into(),
    }
}

#[test]
fn stop_is_on_during_the_copies_until_pressed_and_off_after() {
    assert_eq!(stop_enabled(&copying(), false), Some(true));
    assert_eq!(stop_enabled(&copying(), true), Some(false));
    assert_eq!(stop_enabled(&Progress::Playlists, false), Some(false));
    assert_eq!(stop_enabled(&Progress::Verifying, false), Some(false));
    assert_eq!(
        stop_enabled(&Progress::Removing { done: 0, total: 1 }, false),
        None
    );
    assert_eq!(
        stop_enabled(&Progress::Moving { done: 0, total: 1 }, false),
        None
    );
}

#[test]
fn an_empty_plan_has_nothing_to_apply() {
    assert_eq!(
        to_apply(&Plan::default()),
        "The card is up to date: nothing to apply."
    );
}

// ---------------------------------------------------------------- pure parts

fn copy_op(id: i64, size: u64) -> CopyOp {
    CopyOp {
        track_id: id,
        from: PathBuf::from(format!("/lib/{id}.mp3")),
        to: format!("Music/{id}.mp3"),
        size,
    }
}

// Prevents: the line on what Apply does mis-counting: a track moved through a
// hidden name (a cycle of renames) is two operations but one track, and one
// track is "1 track", not "1 tracks".
#[test]
fn to_apply_counts_tracks_once_and_pluralises() {
    let mut plan = Plan::default();
    plan.copy.push(copy_op(1, 1000));
    // One track, two operations (a hop through a hidden name).
    for (from, to) in [("a", ".hop"), (".hop", "b")] {
        plan.moves.push(MoveOp {
            track_id: 9,
            from: from.into(),
            to: to.into(),
        });
    }
    let one = to_apply(&plan);
    assert!(one.contains("copies 1 track "), "{one}");
    assert!(one.contains("moves 1 track,"), "{one}");

    plan.copy.push(copy_op(2, 500));
    let two = to_apply(&plan);
    assert!(two.contains("copies 2 tracks "), "{two}");
}

// Prevents: Stop being offered for a plan that copies nothing, or not for one
// that only re-copies.
#[test]
fn only_a_plan_with_copies_can_be_stopped() {
    let mut plan = Plan::default();
    assert!(!copies(&plan));
    plan.replace.push(ReplaceOp {
        track_id: 1,
        from: "/lib/1.mp3".into(),
        to: "Music/1.mp3".into(),
        size: 1,
        old_size: 2,
        reason: ReplaceReason::Changed,
    });
    assert!(copies(&plan));
    plan.replace.clear();
    plan.copy.push(copy_op(1, 1));
    assert!(copies(&plan));
}

// ------------------------------------------------------------ the worker

/// A library of `n` tracks and an empty card adopted, the index current.
fn adopted(n: u32) -> World {
    let w = World::new(n);
    w.adopt();
    w
}

fn names(plan: &Plan) -> Vec<String> {
    let mut v: Vec<String> = plan.copy.iter().map(|c| c.to.clone()).collect();
    v.sort();
    v
}

fn root_error(e: &anyhow::Error) -> &fiio_core::Error {
    e.downcast_ref::<fiio_core::Error>()
        .unwrap_or_else(|| panic!("not a fiio-tunes error: {e:#}"))
}

fn apply_all(w: &World, plan: &Plan) -> Result<Applied> {
    apply(
        &w.cfg(),
        &w.paths,
        plan,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
}

// Prevents: the plan the dialog shows not being the card's plan, or planning
// writing to the card: it scans the library into the index, reads the card,
// and writes nothing to it.
#[test]
fn plan_scans_the_library_reads_the_card_and_writes_nothing_to_it() {
    let w = World::new(3);
    // Adopted from an index that has the tracks; then a fourth file appears
    // in the library, which only `plan`'s own scan can know of.
    w.adopt();
    w.env.add("t4.mp3", common::Kind::Mp3, 4);
    let before = w.card_tree();

    let planned = plan(&w.cfg(), &w.paths).unwrap();
    assert_eq!(
        names(&planned.plan),
        [
            "Music/t1.mp3",
            "Music/t2.mp3",
            "Music/t3.mp3",
            "Music/t4.mp3"
        ]
    );
    assert!(planned.fits);
    assert_eq!(planned.mass_removal, None);
    assert_eq!(planned.device_name, "X3");
    // The details name every change, the summary does not need to.
    for t in ["t1.mp3", "t2.mp3", "t3.mp3", "t4.mp3"] {
        assert!(planned.details.contains(t), "{t} in {}", planned.details);
    }
    assert_eq!(store::all_tracks(w.store().conn()).unwrap().len(), 4);
    assert_eq!(w.card_tree(), before, "the card is as it was");
    assert!(w.backups().is_empty());
}

// Prevents: a card that was never adopted being planned for (and a sync then
// copying onto an unknown card).
#[test]
fn plan_refuses_a_card_that_is_not_adopted_and_changes_nothing() {
    let w = World::new(2);
    let e = plan(&w.cfg(), &w.paths).unwrap_err();
    assert!(
        matches!(root_error(&e), fiio_core::Error::NotAdopted(k) if *k == w.key()),
        "{e:#}"
    );
    assert!(w.card_tree().is_empty());
}

// Prevents: a plan being made while another fiio-tunes (the CLI, another
// window) holds the card: it would act on a card that is about to change.
#[test]
fn plan_and_apply_are_refused_while_the_card_lock_is_held() {
    let w = adopted(2);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let before = w.card_tree();
    let _held = fiio_core::lock::card(&w.paths).unwrap();

    let e = plan(&w.cfg(), &w.paths).unwrap_err();
    assert!(
        matches!(root_error(&e), fiio_core::Error::CardBusy),
        "{e:#}"
    );
    let e = apply_all(&w, &planned.plan).unwrap_err();
    assert!(
        matches!(root_error(&e), fiio_core::Error::CardBusy),
        "{e:#}"
    );
    assert_eq!(w.card_tree(), before);
    assert!(w.backups().is_empty());
}

// Prevents: apply "succeeding" without writing, or writing without leaving
// the card, the index and a backup consistent: the files are the library's
// bytes, they are recorded, and the backup is named in the result.
#[test]
fn apply_copies_the_plan_records_it_backs_up_and_reports_progress() {
    let w = adopted(3);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    assert_eq!(planned.plan.copy.len(), 3);
    let seen = Mutex::new(Vec::new());

    let done = apply(
        &w.cfg(),
        &w.paths,
        &planned.plan,
        false,
        &|p| seen.lock().unwrap().push(p),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(
        (done.copied, done.recopied, done.moved, done.removed),
        (3, 0, 0, 0)
    );
    assert_eq!(done.stopped, None);
    assert!(done.skipped.is_empty(), "{:?}", done.skipped);
    let total: u64 = planned.plan.copy.iter().map(|c| c.size).sum();
    assert_eq!(done.bytes, total);
    for i in 1..=3 {
        let on_card = std::fs::read(w.card.join(format!("Music/t{i}.mp3"))).unwrap();
        let in_library = std::fs::read(w.env.music.join(format!("t{i}.mp3"))).unwrap();
        assert!(on_card == in_library, "t{i}.mp3 differs from the library's");
    }
    assert_eq!(
        w.mapped(),
        [
            ("Music/t1.mp3".to_owned(), "t1.mp3".to_owned()),
            ("Music/t2.mp3".to_owned(), "t2.mp3".to_owned()),
            ("Music/t3.mp3".to_owned(), "t3.mp3".to_owned()),
        ]
    );
    // The backup the result names exists, for this card.
    let backups = w.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(done.stamp.as_deref(), Some(backups[0].stamp.as_str()));
    // Progress: the copies in order, each once, of the right total.
    let seen = seen.into_inner().unwrap();
    let copying: Vec<(usize, usize)> = seen
        .iter()
        .filter_map(|p| match p {
            Progress::Copying { done, total, .. } => Some((*done, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(copying, [(0, 3), (1, 3), (2, 3)]);
    assert!(seen.contains(&Progress::Verifying), "{seen:?}");
}

// Prevents: a second Apply of the same plan (a double click, a stale dialog)
// copying again or taking another backup: the plan is no longer the card's.
#[test]
fn applying_the_same_plan_twice_changes_nothing_the_second_time() {
    let w = adopted(2);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    apply_all(&w, &planned.plan).unwrap();
    let (card, backups) = (w.card_tree(), w.backups().len());

    let e = apply_all(&w, &planned.plan).unwrap_err();
    // A refusal of the worker, not an error from the card.
    assert!(e.downcast_ref::<fiio_core::Error>().is_none(), "{e:#}");
    assert!(format!("{e:#}").contains("changed since"), "{e:#}");
    assert_eq!(w.card_tree(), card);
    assert_eq!(w.backups().len(), backups);

    // The new plan is empty, and applying it writes nothing, not even a backup.
    let again = plan(&w.cfg(), &w.paths).unwrap();
    assert!(again.plan.is_empty());
    assert!(again.fits);
    let done = apply_all(&w, &again.plan).unwrap();
    assert_eq!((done.copied, done.removed, done.written), (0, 0, 0));
    assert_eq!(done.stamp, None);
    assert_eq!(w.card_tree(), card);
    assert_eq!(w.backups().len(), backups);
}

// Prevents: Apply writing a plan other than the one shown: a library or a
// playlist changed meanwhile (by another fiio-tunes, which has its own copy
// of the index) makes it refuse, and nothing is written, backed up or
// recorded.
#[test]
fn apply_refuses_a_plan_the_library_or_the_playlists_outgrew() {
    let w = adopted(2);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let before = w.card_tree();

    // Another process scans a new file into the index.
    w.env.add("t3.mp3", common::Kind::Mp3, 3);
    w.scan();
    let e = apply_all(&w, &planned.plan).unwrap_err();
    assert!(format!("{e:#}").contains("changed since"), "{e:#}");

    // Or a playlist appears.
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    w.playlist("Road", &["t1.mp3"]);
    let e = apply_all(&w, &planned.plan).unwrap_err();
    assert!(format!("{e:#}").contains("changed since"), "{e:#}");

    assert_eq!(w.card_tree(), before, "nothing was written");
    assert!(w.backups().is_empty());
    assert!(w.mapped().is_empty());

    // The plan made after the change is the one that applies.
    let fresh = plan(&w.cfg(), &w.paths).unwrap();
    let done = apply_all(&w, &fresh.plan).unwrap();
    assert_eq!(done.copied, 3);
    assert_eq!(done.written, 1);
}

// Prevents: free space, which another program writing elsewhere on the card
// changes between Sync and Apply, making every Apply refuse as "changed".
#[test]
fn apply_ignores_a_difference_in_free_space_only() {
    let w = adopted(2);
    let mut shown = plan(&w.cfg(), &w.paths).unwrap().plan;
    shown.space.peak += 123_456;
    shown.space.needed += 123_456;
    shown.space.device = Some(fiio_core::plan::Space {
        capacity: 1,
        free: 1,
    });

    let done = apply_all(&w, &shown).unwrap();
    assert_eq!(done.copied, 2);
}

// Prevents: Stop losing what was copied or leaving the card in a state the
// next sync cannot continue from: files already copied are kept and
// recorded, the rest is left, and the next plan is exactly the rest.
#[test]
fn stop_keeps_what_was_copied_and_the_next_sync_finishes_the_rest() {
    let w = adopted(3);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let cancel = AtomicBool::new(false);

    // Stop is pressed while the second file is about to be copied.
    let done = apply(
        &w.cfg(),
        &w.paths,
        &planned.plan,
        false,
        &|p| {
            if let Progress::Copying { done: 1, .. } = p {
                cancel.store(true, Ordering::SeqCst);
            }
        },
        &cancel,
    )
    .unwrap();

    assert!(done.stopped.is_some(), "the sync says it was stopped");
    assert_eq!(done.copied, 2);
    assert_eq!(w.mapped().len(), 2, "what was copied is recorded");
    let on_card: Vec<String> = w
        .card_tree()
        .into_iter()
        .filter(|(k, v)| k.ends_with(".mp3") && v.is_some())
        .map(|(k, _)| k)
        .collect();
    assert_eq!(on_card.len(), 2, "{on_card:?}");

    let rest = plan(&w.cfg(), &w.paths).unwrap();
    assert_eq!(rest.plan.copy.len(), 1);
    let done = apply_all(&w, &rest.plan).unwrap();
    assert_eq!((done.copied, done.stopped), (1, None));
    assert_eq!(w.mapped().len(), 3);
}

// Prevents: removals the user was not told about: a plan that drops most of
// what is managed is refused unless allowed, and the refusal changes nothing
// (not even a backup is taken for it).
#[test]
fn a_mass_removal_is_flagged_refused_and_applied_only_when_allowed() {
    // More than five, and more than a tenth, of what is managed.
    let w = adopted(8);
    let first = plan(&w.cfg(), &w.paths).unwrap();
    apply_all(&w, &first.plan).unwrap();
    for i in 2..=8 {
        std::fs::remove_file(w.env.music.join(format!("t{i}.mp3"))).unwrap();
    }
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    assert_eq!(planned.plan.remove.len(), 7);
    assert!(planned.mass_removal.is_some());
    let (card, backups) = (w.card_tree(), w.backups().len());

    let e = apply_all(&w, &planned.plan).unwrap_err();
    assert!(
        matches!(root_error(&e), fiio_core::Error::MassRemoval(_)),
        "{e:#}"
    );
    // The failure came before any backup: the message must not send the user
    // to restore one.
    assert!(!format!("{e:#}").contains("restore"), "{e:#}");
    assert_eq!(w.card_tree(), card);
    assert_eq!(w.backups().len(), backups);

    let done = apply(
        &w.cfg(),
        &w.paths,
        &planned.plan,
        true,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.removed, 7);
    assert_eq!(
        w.mapped(),
        [("Music/t1.mp3".to_owned(), "t1.mp3".to_owned())]
    );
    assert!(w.card.join("Music/t1.mp3").exists());
    assert!(!w.card.join("Music/t2.mp3").exists());
}

/// Makes the card folder read-only for the test, and writable again after.
struct ReadOnly(PathBuf);

impl ReadOnly {
    /// `None` where permission bits do not stop writing (running as root).
    fn new(dir: &Path) -> Option<ReadOnly> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let guard = ReadOnly(dir.to_owned());
        std::fs::File::create(dir.join(".probe"))
            .is_err()
            .then_some(guard)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

// Prevents: a failure after the card was backed up reaching the user without
// the backup: the error must name the stamp and point at restore, and carry
// the cause. The card is made unwritable after the plan, so the write fails.
#[test]
fn a_failure_after_the_backup_names_the_backup_and_keeps_the_cause() {
    let w = adopted(2);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let Some(_ro) = ReadOnly::new(&w.card) else {
        eprintln!("skipped: permission bits do not stop writing here (root?)");
        return;
    };

    let e = apply_all(&w, &planned.plan).unwrap_err();
    let backups = w.backups();
    assert_eq!(backups.len(), 1, "{e:#}");
    let message = format!("{e:#}");
    assert!(message.contains(&backups[0].stamp), "{message}");
    assert!(message.contains("restore"), "{message}");
    assert!(
        matches!(root_error(&e), fiio_core::Error::Io { .. }),
        "the cause is kept: {message}"
    );
    drop(_ro);
    assert!(w.mapped().is_empty(), "no track was recorded");
    assert!(
        tree(&w.card).keys().all(|k| !k.ends_with(".mp3")),
        "no track was copied"
    );
}

// Prevents: the window staying silent after an applied sync (the user may
// have walked away), or announcing one when the plan was empty: an applied
// plan carries a normal "synced" note with its counts, an empty one none.
#[test]
fn apply_returns_a_note_when_it_applied_and_none_for_an_empty_plan() {
    let w = adopted(3);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let done = apply_all(&w, &planned.plan).unwrap();
    let note = done.note.expect("a sync that copied has a note");
    assert_eq!(note.summary, "X3 synced");
    assert!(!note.urgent);
    assert!(note.body.contains("3 tracks"), "{}", note.body);

    let again = plan(&w.cfg(), &w.paths).unwrap();
    assert!(again.plan.is_empty());
    let done = apply_all(&w, &again.plan).unwrap();
    assert_eq!(done.stamp, None);
    assert_eq!(done.note, None);
}

// Prevents: a Stop passing as a plain success in the notification: the note
// says the sync stopped early, and is still not urgent (what was copied
// stands).
#[test]
fn a_stopped_sync_has_a_stopped_early_note() {
    let w = adopted(3);
    let planned = plan(&w.cfg(), &w.paths).unwrap();
    let cancel = AtomicBool::new(false);
    let done = apply(
        &w.cfg(),
        &w.paths,
        &planned.plan,
        false,
        &|p| {
            if let Progress::Copying { done: 1, .. } = p {
                cancel.store(true, Ordering::SeqCst);
            }
        },
        &cancel,
    )
    .unwrap();
    let note = done.note.expect("a stopped sync has a note");
    assert_eq!(note.summary, "X3 sync stopped early");
    assert!(!note.urgent);
    assert!(note.body.contains("2 tracks"), "{}", note.body);
}
