//! One decision per proposal (F7): `adopt::resolve_choices`, the pure
//! resolution, and `adoption::choose`, which puts it into a run read from a
//! card folder in a temp dir. The card is never written.

mod common;

use std::collections::HashMap;
use std::fs;

use common::{Env, FakeCard, Kind};
use fiio_core::Error;
use fiio_core::adopt::{self, Adoption, Conflict, Proposal, Reason};
use fiio_core::adoption::{self, Run};
use fiio_core::store::devices;

const KEY: &str = "folder:test";

fn proposal(key: u64, candidates: &[i64]) -> Proposal {
    Proposal {
        key,
        reason: Reason::SeveralCandidates,
        score: 2,
        candidates: candidates.to_vec(),
        default: candidates.first().copied(),
    }
}

fn picks(p: &[(u64, Option<i64>)]) -> HashMap<u64, Option<i64>> {
    p.iter().copied().collect()
}

fn left_keys(left: &[&Proposal]) -> Vec<u64> {
    left.iter().map(|p| p.key).collect()
}

fn sorted(mut v: Vec<(u64, i64)>) -> Vec<(u64, i64)> {
    v.sort_unstable();
    v
}

// ------------------------------------------------------------- the pure part

// Prevents: a choice being ignored, or the mapping coming back unsorted
// (the saved rows and the report would then depend on the order of the
// proposals).
#[test]
fn each_choice_maps_its_card_file_and_the_mapping_is_sorted_with_the_accepted_ones() {
    let a = Adoption {
        accepted: vec![(5, 50)],
        proposals: vec![proposal(9, &[10, 11]), proposal(2, &[10, 11])],
        ..Adoption::default()
    };
    let (mapping, left) =
        adopt::resolve_choices(&a, &picks(&[(9, Some(10)), (2, Some(11))])).unwrap();
    assert_eq!(mapping, [(2, 11), (5, 50), (9, 10)]);
    assert!(left.is_empty());
}

// Prevents: a file the user left alone being mapped anyway (a default taken
// behind their back), or an id that is not one of the file's candidates
// being mapped: the window can only offer candidates.
#[test]
fn no_entry_none_and_a_stranger_id_leave_the_file_unmanaged() {
    let a = Adoption {
        proposals: vec![
            proposal(1, &[10]),
            proposal(2, &[10]),
            proposal(3, &[10]),
            proposal(4, &[10]),
        ],
        ..Adoption::default()
    };
    // 1: no entry. 2: None. 3: an id no candidate list has. 4: chosen.
    let (mapping, left) =
        adopt::resolve_choices(&a, &picks(&[(2, None), (3, Some(999)), (4, Some(10))])).unwrap();
    assert_eq!(mapping, [(4, 10)]);
    assert_eq!(left_keys(&left), [1, 2, 3]);
}

// Prevents: the one-to-one rule being lost: two card files saved for one
// library file, and the second taken for the first on the next sync.
#[test]
fn a_library_file_chosen_twice_is_refused_naming_both_card_files() {
    let a = Adoption {
        proposals: vec![proposal(7, &[10, 11]), proposal(3, &[10, 11])],
        ..Adoption::default()
    };
    let err = adopt::resolve_choices(&a, &picks(&[(7, Some(10)), (3, Some(10))])).unwrap_err();
    assert_eq!(
        err,
        [Conflict {
            track_id: 10,
            keys: vec![3, 7]
        }]
    );
}

// Prevents: a pick that a file accepted without doubt already has going
// through: the accepted pair is part of the mapping the rule applies to.
#[test]
fn a_library_file_an_accepted_match_has_cannot_be_chosen_again() {
    let a = Adoption {
        accepted: vec![(1, 10)],
        proposals: vec![proposal(2, &[10, 11])],
        ..Adoption::default()
    };
    let err = adopt::resolve_choices(&a, &picks(&[(2, Some(10))])).unwrap_err();
    assert_eq!(
        err,
        [Conflict {
            track_id: 10,
            keys: vec![1, 2]
        }]
    );
    // The other candidate is fine.
    let (mapping, _) = adopt::resolve_choices(&a, &picks(&[(2, Some(11))])).unwrap();
    assert_eq!(mapping, [(1, 10), (2, 11)]);
}

// Prevents: only the first conflict being reported: the user would fix one
// and meet the next at the following Save.
#[test]
fn every_conflict_is_reported_in_library_id_order() {
    let a = Adoption {
        proposals: (1..=4).map(|k| proposal(k, &[10, 20])).collect(),
        ..Adoption::default()
    };
    let err = adopt::resolve_choices(
        &a,
        &picks(&[(1, Some(20)), (2, Some(20)), (3, Some(10)), (4, Some(10))]),
    )
    .unwrap_err();
    assert_eq!(
        err,
        [
            Conflict {
                track_id: 10,
                keys: vec![3, 4]
            },
            Conflict {
                track_id: 20,
                keys: vec![1, 2]
            }
        ]
    );
}

// Prevents: leaving everything unmanaged being refused, or mapping
// something.
#[test]
fn deciding_nothing_for_every_proposal_keeps_only_the_accepted_matches() {
    let a = Adoption {
        accepted: vec![(1, 10)],
        proposals: vec![proposal(2, &[11]), proposal(3, &[11])],
        ..Adoption::default()
    };
    let (mapping, left) = adopt::resolve_choices(&a, &HashMap::new()).unwrap();
    assert_eq!(mapping, [(1, 10)]);
    assert_eq!(left_keys(&left), [2, 3]);
}

// -------------------------------------------------- against a card in a folder

struct World {
    env: Env,
    card: FakeCard,
    _dir: tempfile::TempDir,
}

impl World {
    /// Two library files with identical bytes (a.mp3, b.mp3) and a third
    /// (c.mp3); on the card two identical copies of a.mp3 (x/one.mp3,
    /// y/two.mp3) and a copy of c.mp3, which is matched without doubt.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("card");
        fs::create_dir(&root).unwrap();
        let mut env = Env::new();
        env.add("a.mp3", Kind::Mp3, 1);
        env.add("b.mp3", Kind::Mp3, 1);
        env.add("c.mp3", Kind::Mp3, 3);
        env.scan();
        let mut card = FakeCard::new(root);
        card.copy("x/one.mp3", Kind::Mp3, 1);
        card.copy("y/two.mp3", Kind::Mp3, 1);
        card.copy("z/three.mp3", Kind::Mp3, 3);
        card.age();
        World {
            env,
            card,
            _dir: dir,
        }
    }

    fn run(&self) -> Run {
        adoption::run(
            self.env.store.conn(),
            &self.env.cfg,
            &self.card.root,
            KEY,
            false,
            &|_, _| {},
        )
        .expect("adoption runs")
    }

    fn lib(&self, rel: &str) -> i64 {
        self.env.track(&self.env.music.join(rel)).id
    }
}

fn key_of(r: &Run, rel: &str) -> u64 {
    r.tracks.iter().position(|t| t.file.path == rel).unwrap() as u64
}

// Prevents: `choose` leaving the run as `run` made it (nothing chosen is
// mapped), dropping the accepted match, or a second `choose` building on the
// first instead of on the proposals.
#[test]
fn choose_replaces_the_mapping_and_the_left_proposals_each_time() {
    let mut w = World::new();
    let mut r = w.run();
    let (a, b, c) = (w.lib("a.mp3"), w.lib("b.mp3"), w.lib("c.mp3"));
    let (x, y, z) = (
        key_of(&r, "x/one.mp3"),
        key_of(&r, "y/two.mp3"),
        key_of(&r, "z/three.mp3"),
    );
    assert_eq!(r.mapping, [(z, c)], "only the identical file is mapped");
    assert_eq!(r.left.len(), 2);

    adoption::choose(&mut r, &picks(&[(x, Some(b)), (y, Some(a))])).unwrap();
    assert_eq!(r.mapping, sorted(vec![(x, b), (y, a), (z, c)]));
    assert!(r.left.is_empty());

    adoption::choose(&mut r, &picks(&[(x, None), (y, Some(b))])).unwrap();
    assert_eq!(r.mapping, sorted(vec![(y, b), (z, c)]));
    assert_eq!(r.left.iter().map(|p| p.key).collect::<Vec<_>>(), [x]);
    w.card.assert_untouched();
}

// Prevents: a refused choice half applied: the run must be exactly as it
// was, and the error must name the library file and the card files so the
// window can say which rows clash.
#[test]
fn a_library_file_chosen_twice_is_refused_by_choose_and_the_run_is_unchanged() {
    let mut w = World::new();
    let mut r = w.run();
    let (a, b) = (w.lib("a.mp3"), w.lib("b.mp3"));
    let (x, y) = (key_of(&r, "x/one.mp3"), key_of(&r, "y/two.mp3"));
    // A good choice first, so "unchanged" means the good one stays.
    adoption::choose(&mut r, &picks(&[(x, Some(a)), (y, Some(b))])).unwrap();
    let (mapping, left) = (r.mapping.clone(), r.left.len());

    let err = adoption::choose(&mut r, &picks(&[(x, Some(a)), (y, Some(a))])).unwrap_err();
    match err {
        Error::ChoiceConflicts(lines) => {
            assert_eq!(lines.len(), 1, "{lines:?}");
            let l = &lines[0];
            let lib_path = w.env.music.join("a.mp3").display().to_string();
            assert!(l.contains(&lib_path), "{l}");
            assert!(l.contains("x/one.mp3") && l.contains("y/two.mp3"), "{l}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.mapping, mapping);
    assert_eq!(r.left.len(), left);
    w.card.assert_untouched();
}

// Prevents: a chosen mapping that cannot be saved, or that saves a file the
// user left unmanaged: what is saved is what was chosen, at the card file's
// own path.
#[test]
fn a_chosen_mapping_is_what_is_saved() {
    let mut w = World::new();
    let mut r = w.run();
    let b = w.lib("b.mp3");
    let (x, y) = (key_of(&r, "x/one.mp3"), key_of(&r, "y/two.mp3"));
    adoption::choose(&mut r, &picks(&[(x, Some(b)), (y, None)])).unwrap();
    adoption::save(w.env.store.conn(), &r, None, None, false).unwrap();

    let d = devices::device(w.env.store.conn(), KEY).unwrap().unwrap();
    let mut rows: Vec<(String, i64)> = devices::track_mappings(w.env.store.conn(), d.id)
        .unwrap()
        .into_iter()
        .map(|m| (m.card_path, m.track_id))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("x/one.mp3".to_owned(), b),
            ("z/three.mp3".to_owned(), w.lib("c.mp3"))
        ]
    );
    w.card.assert_untouched();
}
