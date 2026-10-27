// crates/quepaxa/src/dedup.rs, ported to Verus: commit-time deduplication applies an entry
// at most once (verification/verus/prove.sh checks it).
//
// `covered` is what `contains` answers: the entry was seen, or its incarnation is older than
// every one still remembered once INCARNATIONS_KEPT are. Proved, for any state reachable by
// `first_time`: `contains` is exactly `covered`; `first_time` returns true exactly when the
// entry was not covered, and covers it; and nothing covered ever stops being covered. So an
// entry is applied at most once, whatever order and repetition the log decides it in.
// Assumed: ids are below u64::MAX (ProposalIds count up from 0 per incarnation).
//
// Origins are u32 (NodeId) and ids u64 (ProposalId). Departures from the original's code,
// forced by what vstd specifies, are marked DEV; VERIFICATION.md lists them. When dedup.rs
// changes, port the change here. Encoding and decoding are not ported.
use vstd::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use vstd::std_specs::iter::IteratorSpec;
use vstd::std_specs::cmp::OrdSpec;

verus! {

broadcast use {vstd::std_specs::btree::group_btree_axioms, vstd::laws_cmp::group_laws_cmp};

pub const INCARNATIONS_KEPT: usize = 4;

pub struct Window {
    pub below: u64,
    pub above: BTreeSet<u64>,
}

pub struct Dedup {
    pub origins: BTreeMap<u32, BTreeMap<u64, Window>>,
}

pub type WV = (u64, Set<u64>);

pub open spec fn wv(w: Window) -> WV { (w.below, w.above@) }

pub open spec fn incs_view(incs: BTreeMap<u64, Window>) -> Map<u64, WV> {
    incs@.map_values(|w: Window| wv(w))
}

/// Whether a window has seen `id`.
pub open spec fn win_covers(w: WV, id: u64) -> bool {
    id < w.0 || w.1.contains(id)
}

/// What `contains` answers for one origin's incarnations.
pub open spec fn covered_in(incs: Map<u64, WV>, inc: u64, id: u64) -> bool {
    if incs.contains_key(inc) {
        win_covers(incs[inc], id)
    } else {
        incs.dom().len() >= INCARNATIONS_KEPT && forall|k: u64| incs.contains_key(k) ==> inc < k
    }
}

pub open spec fn window_ok(w: WV) -> bool {
    forall|x: u64| w.1.contains(x) ==> x < u64::MAX
}

impl Dedup {
    pub open spec fn view(&self) -> Map<u32, Map<u64, WV>> {
        self.origins@.map_values(|incs: BTreeMap<u64, Window>| incs_view(incs))
    }

    pub open spec fn covered(&self, origin: u32, inc: u64, id: u64) -> bool {
        self.view().contains_key(origin) && covered_in(self.view()[origin], inc, id)
    }

    pub open spec fn inv(&self) -> bool {
        forall|o: u32, k: u64| self.view().contains_key(o) && self.view()[o].contains_key(k)
            ==> window_ok(#[trigger] self.view()[o][k])
    }

    pub fn new() -> (d: Dedup)
        ensures d.inv(), forall|o: u32, k: u64, id: u64| !d.covered(o, k, id),
    {
        Dedup { origins: BTreeMap::new() }
    }

    /// Whether the entry was already seen (without recording it).
    pub fn contains(&self, origin: u32, incarnation: u64, id: u64) -> (r: bool)
        requires self.inv(),
        ensures r == self.covered(origin, incarnation, id),
    {
        let Some(incs) = self.origins.get(&origin) else { return false };
        match incs.get(&incarnation) {
            Some(w) => id < w.below || w.above.contains(&id),
            None => incs.len() >= INCARNATIONS_KEPT && min_key_gt(incs, incarnation),
        }
    }

    /// Records the entry; true the first time it is seen.
    pub fn first_time(&mut self, origin: u32, incarnation: u64, id: u64) -> (r: bool)
        requires old(self).inv(), id < u64::MAX,
        ensures
            final(self).inv(),
            r == !old(self).covered(origin, incarnation, id),
            final(self).covered(origin, incarnation, id),
            forall|o: u32, k: u64, x: u64| old(self).covered(o, k, x) ==> final(self).covered(o, k, x),
    {
        // DEV: `entry(origin).or_default()` (no vstd spec) becomes remove-then-insert, here and
        // for the incarnation's window below.
        let ghost v0 = self.view();
        let mut incs = match self.origins.remove(&origin) { Some(i) => i, None => BTreeMap::new() };
        assert forall|k: u64| #[trigger] incs_view(incs).contains_key(k) implies window_ok(incs_view(incs)[k]) by {
            if v0.contains_key(origin) {
                assert(incs_view(incs) == v0[origin]);
                assert(v0.contains_key(origin) && v0[origin].contains_key(k));
            } else {
                assert(incs_view(incs) =~= Map::<u64, WV>::empty());
            }
        }
        let ghost i0 = incs_view(incs);
        let r = Self::first_time_in(&mut incs, incarnation, id);
        let ghost i1 = incs_view(incs);
        self.origins.insert(origin, incs);
        proof {
            assert(self.view() =~= v0.insert(origin, i1));
            assert(v0.contains_key(origin) ==> i0 == v0[origin]);
            assert(!v0.contains_key(origin) ==> i0 =~= Map::<u64, WV>::empty());
            assert forall|o: u32, k: u64| self.view().contains_key(o) && self.view()[o].contains_key(k)
                implies window_ok(#[trigger] self.view()[o][k]) by {
                if o != origin {
                    assert(self.view()[o] == v0[o]);
                }
            }
            assert forall|o: u32, k: u64, x: u64| old(self).covered(o, k, x) implies self.covered(o, k, x) by {
                if o != origin {
                    assert(self.view()[o] == v0[o]);
                }
            }
        }
        r
    }

    fn first_time_in(incs: &mut BTreeMap<u64, Window>, incarnation: u64, id: u64) -> (r: bool)
        requires id < u64::MAX,
            forall|k: u64| #[trigger] incs_view(*old(incs)).contains_key(k) ==> window_ok(incs_view(*old(incs))[k]),
        ensures
            forall|k: u64| #[trigger] incs_view(*final(incs)).contains_key(k) ==> window_ok(incs_view(*final(incs))[k]),
            r == !covered_in(incs_view(*old(incs)), incarnation, id),
            covered_in(incs_view(*final(incs)), incarnation, id),
            forall|k: u64, x: u64| covered_in(incs_view(*old(incs)), k, x) ==> covered_in(incs_view(*final(incs)), k, x),
    {
        assert(vstd::laws_cmp::obeys_cmp::<u64>());
        let ghost s0 = incs_view(*incs);
        let ghost mut evicted: Option<u64> = None;
        if incs.len() >= INCARNATIONS_KEPT && !incs.contains_key(&incarnation) {
            if min_key_gt(incs, incarnation) {
                return false;
            }
            // DEV: `pop_first()` (no vstd spec) removes the smallest key found by `keys()`.
            let oldest = min_key(incs);
            incs.remove(&oldest);
            proof {
                evicted = Some(oldest);
                assert(incarnation > oldest);
            }
        }
        let ghost s1 = incs_view(*incs);
        assert(s1 =~= match evicted { Some(o) => s0.remove(o), None => s0 });
        let mut w = match incs.remove(&incarnation) {
            Some(w) => w,
            None => Window { below: 0, above: BTreeSet::new() },
        };
        let ghost s2 = incs_view(*incs);
        assert(s2 =~= s1.remove(incarnation));
        let ghost w0 = wv(w);
        assert(s1.contains_key(incarnation) ==> w0 == s1[incarnation]);
        assert(!s1.contains_key(incarnation) ==> w0.1 =~= Set::<u64>::empty() && w0.0 == 0);
        assert(s1.contains_key(incarnation) == s0.contains_key(incarnation));
        assert(window_ok(w0));
        let fresh = id >= w.below && w.above.insert(id);
        let ghost w1 = wv(w);
        assert(fresh == !win_covers(w0, id));
        assert(fresh ==> w1 == (w0.0, w0.1.insert(id)));
        assert(!fresh ==> w1 == w0);
        if !fresh {
            incs.insert(incarnation, w);
            proof { Self::lemma_done(s0, s1, incs_view(*incs), evicted, incarnation, id, w0, w1); }
            return false;
        }
        // DEV: `while w.above.remove(&w.below) { .. }` becomes a loop with the call in its body.
        loop
            invariant
                forall|x: u64| w.above@.contains(x) ==> x < u64::MAX,
                forall|x: u64| #[trigger] win_covers(w1, x) == win_covers(wv(w), x),
            decreases w.above@.len(),
        {
            let ghost b = w.below;
            let ghost a = w.above@;
            let removed = w.above.remove(&w.below);
            assert(w.above@ == a.remove(b) && removed == a.contains(b));
            if !removed {
                assert(w.above@ =~= a);
                break;
            }
            w.below = w.below + 1;
            assert forall|x: u64| #[trigger] win_covers(w1, x) == win_covers(wv(w), x) by {
                assert(win_covers(w1, x) == win_covers((b, a), x));
            }
        }
        let ghost wf = wv(w);
        assert(window_ok(wf));
        assert forall|x: u64| #[trigger] win_covers(wf, x) == (win_covers(w0, x) || x == id) by {
            assert(win_covers(w1, x) == win_covers(wf, x));
        }
        incs.insert(incarnation, w);
        proof { Self::lemma_done(s0, s1, incs_view(*incs), evicted, incarnation, id, w0, wf); }
        true
    }

    /// The incarnations after `first_time_in`: `s1` is `s0` less the evicted oldest, the
    /// result is `s1` with the incarnation's window set to `wf`, whose seen ids are `w0`'s
    /// plus `id`.
    proof fn lemma_done(s0: Map<u64, WV>, s1: Map<u64, WV>, fin: Map<u64, WV>, evicted: Option<u64>,
        inc: u64, id: u64, w0: WV, wf: WV)
        requires
            forall|k: u64| #[trigger] s0.contains_key(k) ==> window_ok(s0[k]),
            window_ok(wf),
            s1 == match evicted { Some(o) => s0.remove(o), None => s0 },
            fin =~= s1.insert(inc, wf),
            s1.contains_key(inc) == s0.contains_key(inc),
            s1.contains_key(inc) ==> w0 == s1[inc],
            !s1.contains_key(inc) ==> !win_covers(w0, id) || w0 == wf,
            forall|x: u64| #[trigger] win_covers(wf, x) == (win_covers(w0, x) || x == id),
            match evicted {
                Some(o) => {
                    &&& s0.contains_key(o) && !s0.contains_key(inc) && o < inc
                    &&& s0.dom().len() >= INCARNATIONS_KEPT
                    &&& forall|j: u64| s0.contains_key(j) ==> o <= j
                },
                None => !s0.contains_key(inc) ==> s0.dom().len() < INCARNATIONS_KEPT,
            },
        ensures
            forall|k: u64| #[trigger] fin.contains_key(k) ==> window_ok(fin[k]),
            covered_in(fin, inc, id),
            forall|k: u64, x: u64| covered_in(s0, k, x) ==> covered_in(fin, k, x),
    {
        assert(fin.dom().len() >= s0.dom().len()) by {
            match evicted {
                Some(o) => {
                    assert(s1.dom().len() == s0.dom().len() - 1);
                    assert(fin.dom() =~= s1.dom().insert(inc));
                }
                None => {
                    assert(fin.dom() =~= s0.dom().insert(inc));
                }
            }
        }
        assert forall|k: u64, x: u64| covered_in(s0, k, x) implies covered_in(fin, k, x) by {
            if k == inc {
                if s0.contains_key(inc) {
                    assert(win_covers(wf, x));
                }
            } else if s0.contains_key(k) && (evicted != Some(k)) {
                assert(fin[k] == s0[k]);
            } else {
                assert forall|j: u64| fin.contains_key(j) implies k < j by {
                    if j != inc {
                        assert(s0.contains_key(j));
                    }
                }
            }
        }
    }
}

/// The smallest incarnation kept. DEV: `keys().next()`, with the iterator's spec.
fn min_key(incs: &BTreeMap<u64, Window>) -> (k: u64)
    requires incs@.dom().len() > 0,
    ensures incs@.contains_key(k), forall|j: u64| incs@.contains_key(j) ==> k <= j,
{
    proof {
        broadcast use vstd::std_specs::btree::axiom_key_obeys_cmp_spec_meaning;
        assert(vstd::std_specs::btree::key_obeys_cmp_spec::<u64>());
    }
    let mut keys = incs.keys();
    let ghost r0 = keys.remaining();
    assert(r0.unref().to_set() == incs@.dom());
    assert(r0.len() > 0) by {
        if r0.len() == 0 {
            assert(r0.unref() =~= Seq::<u64>::empty());
            assert(r0.unref().to_set() =~= Set::<u64>::empty());
        }
    }
    match keys.next() {
        Some(k) => {
            proof {
                assert(*k == r0.unref()[0]);
                assert(r0.unref().contains(*k));
                broadcast use vstd::std_specs::btree::axiom_increasing_seq_meaning,
                    vstd::laws_cmp::lemma_ref_obeys_cmp_spec;
                assert(vstd::std_specs::btree::increasing_seq(r0));
                assert(vstd::laws_cmp::obeys_cmp::<&u64>());
                vstd::std_specs::btree::axiom_increasing_seq_meaning(r0);
                assert forall|j: u64| incs@.contains_key(j) implies *k <= j by {
                    assert(r0.unref().to_set().contains(j));
                    assert(r0.unref().contains(j));
                    let i = choose|i: int| 0 <= i < r0.unref().len() && r0.unref()[i] == j;
                    assert(r0.unref().len() == r0.len() && *r0[i] == j && *r0[0] == *k);
                    if i > 0 {
                        assert(<&u64 as OrdSpec>::cmp_spec(&r0[0], &r0[i]) is Less);
                    }
                }
            }
            *k
        },
        None => 0,
    }
}

/// Whether `inc` is below every incarnation kept (`keys().next().is_some_and(|&o| inc < o)`).
fn min_key_gt(incs: &BTreeMap<u64, Window>, inc: u64) -> (r: bool)
    ensures r == (incs@.dom().len() > 0 && forall|j: u64| incs@.contains_key(j) ==> inc < j),
{
    if incs.len() == 0 {
        return false;
    }
    inc < min_key(incs)
}

} // verus!
