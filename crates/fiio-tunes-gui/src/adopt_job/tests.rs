//! The adoption worker: its pure parts, and `prepare` and `save` against a
//! card that is a temp folder (`device.mountpoint` with `allow_folder`). The
//! library, the index and the data folder are temp dirs too; the fixtures are
//! the core crate's integration-test helpers. Nothing is written to the card.

// The worker tests each include the fixtures: one copy per test module.
#[allow(clippy::duplicate_mod)]
#[path = "../../../fiio-core/tests/common/mod.rs"]
mod common;
#[allow(clippy::duplicate_mod)]
#[path = "../test_world.rs"]
mod world;

use std::sync::Mutex;

use common::{Kind, fixture, id3v24_text, mp3_with_id3v2};
use fiio_core::adopt::Reason;
use world::World;

use super::*;

fn row(key: u64, choices: &[Option<i64>]) -> ReviewRow {
    ReviewRow {
        key,
        card: String::new(),
        album: String::new(),
        detail: String::new(),
        why: "",
        choices: choices
            .iter()
            .map(|c| Choice {
                track_id: *c,
                label: String::new(),
            })
            .collect(),
        suggested: 0,
    }
}

#[test]
fn choices_take_the_pick_of_each_row_and_out_of_range_leaves_it_unmanaged() {
    let rows = [
        row(3, &[None, Some(10), Some(11)]),
        row(7, &[None, Some(12)]),
    ];
    let got = choices(&rows, &[2, 5]);
    assert_eq!(got[&3], Some(11));
    assert_eq!(got[&7], None);
}

// ------------------------------------------------------------ the worker

fn put(w: &World, rel: &str, bytes: &[u8]) {
    let p = w.card.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn put_fixture(w: &World, rel: &str, variant: u32) {
    put(w, rel, &std::fs::read(fixture(Kind::Mp3, variant)).unwrap());
}

/// Library a.mp3 and b.mp3 (identical bytes) and c.mp3; on the card two
/// identical copies, x/one.mp3 and y/two.mp3, which two library files claim
/// (two proposals), and z/three.mp3, which is c.mp3 (matched without doubt).
fn twins() -> World {
    let w = World::new(0);
    w.env.add("a.mp3", Kind::Mp3, 1);
    w.env.add("b.mp3", Kind::Mp3, 1);
    w.env.add("c.mp3", Kind::Mp3, 3);
    put_fixture(&w, "x/one.mp3", 1);
    put_fixture(&w, "y/two.mp3", 1);
    put_fixture(&w, "z/three.mp3", 3);
    w
}

fn prepared(w: &World) -> Review {
    prepare(&w.cfg(), &w.paths, &|_, _| {}).unwrap_or_else(|e| panic!("prepare: {e:#}"))
}

fn refused(r: Result<Review>) -> anyhow::Error {
    match r {
        Err(e) => e,
        Ok(_) => panic!("expected a refusal"),
    }
}

fn lib_name(r: &Review, id: i64) -> String {
    r.run.library[&id]
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn row_of<'a>(r: &'a Review, card: &str) -> &'a ReviewRow {
    r.rows
        .iter()
        .find(|row| row.card == card)
        .unwrap_or_else(|| panic!("no row for {card}"))
}

/// The pick of each row that `adopt --accept-proposals` takes.
fn suggested(r: &Review) -> Vec<usize> {
    r.rows.iter().map(|row| row.suggested).collect()
}

fn device_row(w: &World) -> Option<fiio_core::store::devices::DeviceRow> {
    fiio_core::store::devices::device(w.store().conn(), &w.key()).unwrap()
}

// Prevents: the review grid not matching what adoption found: one row per
// proposal (not per card file), "leave it unmanaged" first, every candidate
// offered, and the pre-selected pick the one `--accept-proposals` takes. The
// card is only read, and the full report is written for the user.
#[test]
fn prepare_gives_one_row_per_proposal_writes_the_report_and_touches_neither_card_nor_index_rows() {
    let w = twins();
    let before = w.card_tree();
    let seen = Mutex::new(Vec::new());

    let review = prepare(&w.cfg(), &w.paths, &|d, t| {
        seen.lock().unwrap().push((d, t))
    })
    .unwrap_or_else(|e| panic!("{e:#}"));

    assert_eq!(review.run.key, w.key());
    assert_eq!(review.rows.len(), 2, "z/three.mp3 is matched, not proposed");
    let (x, y) = (row_of(&review, "x/one.mp3"), row_of(&review, "y/two.mp3"));
    for row in [x, y] {
        assert_eq!(row.choices[0].track_id, None, "unmanaged comes first");
        let ids: Vec<i64> = row.choices[1..].iter().filter_map(|c| c.track_id).collect();
        assert_eq!(ids.len(), 2);
        let mut names: Vec<String> = ids.iter().map(|id| lib_name(&review, *id)).collect();
        names.sort();
        assert_eq!(names, ["a.mp3", "b.mp3"]);
        // Each candidate's label names its file.
        for (c, id) in row.choices[1..].iter().zip(&ids) {
            assert!(c.label.contains(&lib_name(&review, *id)), "{}", c.label);
        }
    }
    // The pre-selected picks are two different library files.
    let pick = |row: &ReviewRow| row.choices[row.suggested].track_id;
    assert!(pick(x).is_some() && pick(y).is_some());
    assert_ne!(
        pick(x),
        pick(y),
        "accepting proposals does not pick one file twice"
    );
    // What `adopt --accept-proposals` maps is what the rows pre-select.
    let accepted = adopt::resolve(&review.run.adoption, true).0;
    for row in [x, y] {
        assert!(accepted.contains(&(row.key, pick(row).unwrap())));
    }

    assert_eq!(
        seen.lock().unwrap().last(),
        Some(&(3, 3)),
        "three card files read"
    );
    // The report is the one `adoption::report` makes, where the summary says.
    assert_eq!(review.report, w.paths.data_dir.join("adopt-report.txt"));
    assert_eq!(
        std::fs::read_to_string(&review.report).unwrap(),
        adoption::report(&review.run)
    );
    assert_eq!(w.card_tree(), before, "the card was only read");
    assert!(device_row(&w).is_none(), "nothing is adopted until Save");
    assert!(w.backups().is_empty());
}

// Prevents: an X3 that is adopted already being adopted again from the
// window, which replaces its mappings: only `adopt --force` may. The refusal
// comes before anything is read or written, the report included.
#[test]
fn prepare_refuses_an_x3_that_is_adopted_and_writes_nothing() {
    let w = twins();
    w.adopt();
    let mapped = w.mapped();
    assert_eq!(mapped.len(), 3);
    std::fs::create_dir_all(&w.paths.data_dir).unwrap();
    let report = w.paths.data_dir.join("adopt-report.txt");
    std::fs::write(&report, "the earlier report").unwrap();
    let before = w.card_tree();

    let e = refused(prepare(&w.cfg(), &w.paths, &|_, _| {}));
    assert!(format!("{e:#}").contains("already adopted"), "{e:#}");
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        "the earlier report"
    );
    assert_eq!(w.mapped(), mapped);
    assert_eq!(w.card_tree(), before);
}

// Prevents: "adopted" meaning "has a device row": an X3 whose adoption mapped
// nothing (an empty card) can be adopted again once it has files; refusing it
// would leave it unmanageable from the window.
#[test]
fn prepare_accepts_an_x3_adopted_with_nothing_mapped() {
    let w = World::new(2);
    w.adopt();
    assert!(device_row(&w).is_some());
    assert!(w.mapped().is_empty());
    put_fixture(&w, "mine.mp3", 1);
    let review = prepared(&w);
    assert!(review.rows.is_empty());
    assert_eq!(review.run.mapping.len(), 1);
}

// Prevents: an empty library (a disk that is not mounted) being adopted
// against: the worker's scan marks nothing, and adoption refuses.
#[test]
fn prepare_with_an_empty_library_fails_and_writes_no_report() {
    let w = World::new(0);
    put_fixture(&w, "x.mp3", 1);
    let e = refused(prepare(&w.cfg(), &w.paths, &|_, _| {}));
    assert!(
        matches!(
            e.downcast_ref::<fiio_core::Error>(),
            Some(fiio_core::Error::EmptyIndex)
        ),
        "{e:#}"
    );
    assert!(!w.paths.data_dir.join("adopt-report.txt").exists());
}

// Prevents: a weak match (same size and duration, other tags) being
// pre-selected: with no evidence, the default is to leave the file alone.
#[test]
fn a_weak_proposal_is_not_preselected() {
    let w = World::new(0);
    let tags = |t: &[u8], a: &[u8]| {
        mp3_with_id3v2(1, &[id3v24_text("TIT2", 3, t), id3v24_text("TPE1", 3, a)])
    };
    let lib = tags(b"AAAAAA", b"BBBBBB");
    let card = tags(b"CCCCCC", b"DDDDDD");
    assert_eq!(lib.len(), card.len());
    w.env.add_bytes("w.mp3", &lib);
    put(&w, "weak.mp3", &card);

    let review = prepared(&w);
    assert_eq!(review.rows.len(), 1);
    let row = &review.rows[0];
    assert_eq!(row.choices.len(), 2);
    assert_eq!(row.suggested, 0);
    assert_eq!(row.choices[row.suggested].track_id, None);
    assert_eq!(review.run.adoption.proposals[0].reason, Reason::WeakTags);
    assert!(!row.why.is_empty());
}

// Prevents: Save writing something other than what the grid shows: the
// pre-selected picks are saved as they are, the matched file with them, and
// the counts are the ones the window reports.
#[test]
fn save_records_the_picks_and_counts_what_it_left() {
    let w = twins();
    let review = prepared(&w);
    let picks = suggested(&review);

    let saved = save(&w.cfg(), &w.paths, review, &picks).unwrap();

    assert_eq!(
        saved,
        Saved {
            tracks: 3,
            playlists: 0,
            unmanaged: 0,
            to_copy: 0
        }
    );
    let mapped = w.mapped();
    let card: Vec<&str> = mapped.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(card, ["x/one.mp3", "y/two.mp3", "z/three.mp3"]);
    let mut twins: Vec<&str> = mapped[..2].iter().map(|(_, l)| l.as_str()).collect();
    twins.sort();
    assert_eq!(twins, ["a.mp3", "b.mp3"]);
    assert_eq!(mapped[2].1, "c.mp3");
    assert!(device_row(&w).is_some());
    // And it is adopted now: the window refuses a second adoption.
    let again = refused(prepare(&w.cfg(), &w.paths, &|_, _| {}));
    assert!(
        format!("{again:#}").contains("already adopted"),
        "{again:#}"
    );
}

// Prevents: "leave it unmanaged" mapping anyway, or being counted as
// something else: those files stay on the card unrecorded, and the library
// files they would have held are left for the next sync to copy.
#[test]
fn save_with_every_proposal_left_unmanaged_maps_only_the_matched_file() {
    let w = twins();
    let review = prepared(&w);
    let picks = vec![0; review.rows.len()];

    let saved = save(&w.cfg(), &w.paths, review, &picks).unwrap();

    assert_eq!(
        saved,
        Saved {
            tracks: 1,
            playlists: 0,
            unmanaged: 2,
            to_copy: 2
        }
    );
    assert_eq!(w.mapped(), [("z/three.mp3".to_owned(), "c.mp3".to_owned())]);
}

// Prevents: a card file with no library counterpart being left out of the
// count of files left on the card, or counted as one to copy.
#[test]
fn save_counts_a_card_file_with_no_library_file_as_unmanaged() {
    let w = twins();
    // Another length, so another size: no library file resembles it.
    put(
        &w,
        "q/stranger.mp3",
        &std::fs::read(common::fixture_secs(Kind::Mp3, 9, 3)).unwrap(),
    );
    let review = prepared(&w);
    assert_eq!(review.rows.len(), 2, "the stranger is not a proposal");
    let picks = suggested(&review);

    let saved = save(&w.cfg(), &w.paths, review, &picks).unwrap();

    assert_eq!(
        saved,
        Saved {
            tracks: 3,
            playlists: 0,
            unmanaged: 1,
            to_copy: 0
        }
    );
    assert!(w.mapped().iter().all(|(c, _)| c != "q/stranger.mp3"));
}

// Prevents: a library file saved for two card files: refused with the
// conflict, and nothing saved, device row included.
#[test]
fn save_refuses_a_library_file_picked_twice_and_saves_nothing() {
    let w = twins();
    let review = prepared(&w);
    let same = vec![1; review.rows.len()];
    // Candidate 1 of each row is the same best candidate.
    assert_eq!(
        review.rows[0].choices[1].track_id,
        review.rows[1].choices[1].track_id
    );
    assert_eq!(conflicts(&review, &same).len(), 1);

    let e = match save(&w.cfg(), &w.paths, review, &same) {
        Err(e) => e,
        Ok(s) => panic!("saved {s:?}"),
    };
    assert!(
        matches!(
            e.downcast_ref::<fiio_core::Error>(),
            Some(fiio_core::Error::ChoiceConflicts(_))
        ),
        "{e:#}"
    );
    assert!(device_row(&w).is_none(), "no device row");
    assert!(w.mapped().is_empty());
}

// Prevents: the conflict list being wrong in either direction: a conflict
// missed (Save would then fail late) or reported where there is none (Save
// would be blocked for nothing).
#[test]
fn conflicts_lists_a_file_picked_twice_and_nothing_for_distinct_or_missing_picks() {
    let w = twins();
    let review = prepared(&w);
    let id = |row: usize, pick: usize| review.rows[row].choices[pick].track_id;

    let same = conflicts(&review, &[1, 1]);
    assert_eq!(same.len(), 1, "{same:?}");
    let shared = lib_name(&review, id(0, 1).unwrap());
    assert!(same[0].contains(&shared), "{} names {shared}", same[0]);

    assert!(
        conflicts(&review, &[1, 2]).is_empty(),
        "two different files"
    );
    assert!(conflicts(&review, &[0, 0]).is_empty(), "both unmanaged");
    assert!(conflicts(&review, &[1, 0]).is_empty(), "one unmanaged");
    assert!(
        conflicts(&review, &[7, 9]).is_empty(),
        "out of range is unmanaged"
    );
    assert!(conflicts(&review, &[]).is_empty(), "no picks yet");
}

// Prevents: a pick that a file matched without doubt already has being
// allowed: the window must say another card file already holds it, and a
// pick of two rows on top of that must say so too. The matcher does not
// offer a file it has accepted for another as a candidate, so the state is
// made by hand: z/three.mp3 is taken to have matched the library file a
// candidate of x and y also names.
#[test]
fn a_file_an_accepted_match_already_has_conflicts_with_a_pick_of_it() {
    let w = twins();
    let mut review = prepared(&w);
    let candidate = review.rows[0].choices[1].track_id.unwrap();
    let z = review
        .run
        .tracks
        .iter()
        .position(|t| t.file.path == "z/three.mp3")
        .unwrap() as u64;
    review.run.adoption.accepted.retain(|(k, _)| *k != z);
    review.run.adoption.accepted.push((z, candidate));
    let name = lib_name(&review, candidate);

    let one = conflicts(&review, &[1, 0]);
    assert_eq!(one.len(), 1, "{one:?}");
    assert!(
        one[0].contains(&name) && one[0].contains("already matched"),
        "{}",
        one[0]
    );

    let two = conflicts(&review, &[1, 1]);
    assert_eq!(two.len(), 1, "{two:?}");
    assert!(
        two[0].contains(&name) && two[0].contains("already matched"),
        "{}",
        two[0]
    );
    assert_ne!(one, two, "the line says how many rows picked it");

    // The other candidate of the second row is free.
    assert!(conflicts(&review, &[0, 2]).is_empty());
}

// Prevents: Save writing the adoption of a card that is not the one reviewed
// (another card plugged meanwhile, the first unplugged): checked against the
// card connected now, and nothing saved.
#[test]
fn save_refuses_another_card_or_none_and_saves_nothing() {
    assert!(
        fiio_core::device::detection_off(),
        "run through cargo: .cargo/config.toml sets FIIO_TUNES_NO_DETECT"
    );
    let w = twins();
    let picks = |r: &Review| suggested(r);

    // Another folder is the card now.
    let review = prepared(&w);
    let other = w.card.parent().unwrap().join("other-card");
    std::fs::create_dir(&other).unwrap();
    let mut cfg = w.cfg();
    cfg.device.mountpoint = other;
    let p = picks(&review);
    let e = match save(&cfg, &w.paths, review, &p) {
        Err(e) => e,
        Ok(s) => panic!("saved {s:?}"),
    };
    assert!(format!("{e:#}").contains("not the one reviewed"), "{e:#}");
    assert!(device_row(&w).is_none());

    // No card at all: nothing is configured and nothing is detected.
    let review = prepared(&w);
    let mut cfg = w.cfg();
    cfg.device.mountpoint = PathBuf::new();
    let p = picks(&review);
    let e = match save(&cfg, &w.paths, review, &p) {
        Err(e) => e,
        Ok(s) => panic!("saved {s:?}"),
    };
    assert!(format!("{e:#}").contains("no longer connected"), "{e:#}");
    assert!(device_row(&w).is_none());
    assert!(w.mapped().is_empty());
}
