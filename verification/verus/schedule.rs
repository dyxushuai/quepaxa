// crates/quepaxa/src/schedule.rs, ported to Verus: the window rule at the code level
// (verification/verus/prove.sh checks it).
//
// The order in force for slot k is the last history entry whose first slot is at most k.
// Proved: feeding decided slot s (`observe`, then any `reconfigure`s for it) never changes
// the order of a slot in [s, s + LAG). Slots are fed in order, so once every slot up to
// k - LAG is fed, slot k's members and leader are fixed: every replica that drives k
// (only below its commit index + LAG) computes the same ones. Every slot has an order, and
// every order is non-empty, so `leader` never indexes out of bounds.
// Assumed: slots stay at most MAX_SLOT (the replica rejects larger ones), and a schedule
// restored from a snapshot satisfies `inv` for the slots it covers (Schedule::fits, checked
// when the snapshot's core state is decoded).
//
// Nodes are u32 and change keys u64. Departures from the original's code, forced by what
// vstd specifies, are marked DEV; VERIFICATION.md lists them. When schedule.rs changes,
// port the change here. Encoding, decoding, `knows`, `everyone` and `position` are not
// ported.
use vstd::prelude::*;

verus! {

pub const LAG: u64 = 64;
pub const ROTATE_AFTER: u32 = 8;
pub const MAX_SLOT: u64 = 0xffff_fffe_ffff_ffff;

pub type Entry = (u64, Seq<u32>);

pub open spec fn hv(h: Vec<(u64, Vec<u32>)>) -> Seq<Entry> {
    h@.map_values(|e: (u64, Vec<u32>)| (e.0, e.1@))
}

/// Entry i is the one in force for slot k.
pub open spec fn in_force(h: Seq<Entry>, k: u64, i: int) -> bool {
    &&& 0 <= i < h.len()
    &&& (i == 0 || h[i].0 <= k)
    &&& (i + 1 == h.len() || h[i + 1].0 > k)
}

pub open spec fn well_formed(h: Seq<Entry>) -> bool {
    &&& h.len() > 0
    &&& forall|i: int, j: int| 0 <= i < j < h.len() ==> h[i].0 < h[j].0
    &&& forall|i: int| 0 <= i < h.len() ==> (#[trigger] h[i]).1.len() > 0
}

/// Slot k keeps its order from `a` to `b`, and has one in `b`.
pub open spec fn kept_at(a: Seq<Entry>, b: Seq<Entry>, k: u64) -> bool {
    &&& exists|j: int| in_force(b, k, j)
    &&& forall|i: int, j: int| in_force(a, k, i) && in_force(b, k, j) ==> a[i].1 == b[j].1
}

/// Slots in [s, s + LAG) keep their order from `a` to `b`.
pub open spec fn window_kept(a: Seq<Entry>, b: Seq<Entry>, s: u64) -> bool {
    forall|k: u64| s <= k < s + LAG ==> #[trigger] kept_at(a, b, k)
}

pub struct Schedule {
    pub history: Vec<(u64, Vec<u32>)>,
    pub streak: u32,
    pub quiet_until: u64,
    pub epoch: u64,
    pub epoch_from: u64,
    pub settled: Vec<u64>,
    /// The last slot fed, if any.
    pub fed: Ghost<Option<u64>>,
}

/// Searching down from entry c, past entries that start after k, finds the one in force.
proof fn lemma_find(h: Seq<Entry>, k: u64, c: int)
    requires well_formed(h), 0 <= c < h.len(), forall|j: int| c < j < h.len() ==> h[j].0 > k,
    ensures exists|i: int| in_force(h, k, i),
    decreases c,
{
    if c == 0 || h[c].0 <= k {
        assert(in_force(h, k, c));
    } else {
        lemma_find(h, k, c - 1);
    }
}

/// In a well-formed history, every slot has exactly one entry in force.
proof fn lemma_in_force(h: Seq<Entry>, k: u64)
    requires well_formed(h),
    ensures
        exists|i: int| in_force(h, k, i),
        forall|i: int, j: int| in_force(h, k, i) && in_force(h, k, j) ==> i == j,
{
    lemma_find(h, k, h.len() - 1);
    assert forall|a: int, b: int| in_force(h, k, a) && in_force(h, k, b) implies a == b by {
        if a < b {
            if a + 1 < b { assert(h[a + 1].0 < h[b].0); }
        } else if b < a {
            if b + 1 < a { assert(h[b + 1].0 < h[a].0); }
        }
    }
}

impl Schedule {
    pub open spec fn view(&self) -> Seq<Entry> { hv(self.history) }

    pub open spec fn inv(&self) -> bool {
        &&& well_formed(self.view())
        &&& self.streak < ROTATE_AFTER
        &&& match self.fed@ {
                Some(f) => f <= MAX_SLOT && self.view()[0].0 <= f
                    && forall|i: int| 0 <= i < self.view().len() ==> (#[trigger] self.view()[i]).0 <= f + LAG,
                None => self.view().len() == 1 && self.view()[0].0 == 0,
            }
    }

    pub fn with_order(order: Vec<u32>) -> (s: Schedule)
        requires order@.len() > 0,
        ensures s.inv(), s.fed@ is None,
    {
        let s = Schedule {
            history: vec![(0, order)],
            streak: 0,
            quiet_until: 0,
            epoch: 0,
            epoch_from: 0,
            settled: Vec::new(),
            fed: Ghost(None),
        };
        assert(s.view().len() == 1 && s.view()[0].1.len() > 0);
        s
    }

    /// The index of the entry in force for `slot`. DEV: a scan for `partition_point`.
    fn order_index(&self, slot: u64) -> (i: usize)
        requires well_formed(self.view()),
        ensures in_force(self.view(), slot, i as int),
    {
        let n = self.history.len();
        let mut i: usize = 0;
        while i + 1 < n && self.history[i + 1].0 <= slot
            invariant
                well_formed(self.view()), n == self.view().len(), i < n,
                i == 0 || self.view()[i as int].0 <= slot,
            decreases n - i,
        {
            i = i + 1;
        }
        i
    }

    pub fn leader(&self, slot: u64) -> (l: u32)
        requires self.inv(),
        ensures exists|i: int| in_force(self.view(), slot, i) && l == self.view()[i].1[0],
    {
        let i = self.order_index(slot);
        assert(self.view()[i as int].1.len() > 0);
        self.history[i].1[0]
    }

    /// Feeds each decided slot exactly once, in slot order, with its value's origin.
    pub fn observe(&mut self, slot: u64, origin: u32)
        requires
            old(self).inv(), slot <= MAX_SLOT,
            match old(self).fed@ { Some(f) => f < slot, None => true },
        ensures
            final(self).inv(), final(self).fed@ == Some(slot),
            window_kept(old(self).view(), final(self).view(), slot),
    {
        let ghost h0 = self.view();
        proof { self.lemma_fed(slot); }
        if slot < self.quiet_until {
            self.fed = Ghost(Some(slot));
            proof { lemma_same(h0, slot); }
            return;
        }
        if origin == self.leader(slot) {
            self.streak = 0;
            self.fed = Ghost(Some(slot));
            proof { lemma_same(h0, slot); }
            return;
        }
        self.streak = self.streak + 1;
        if self.streak >= ROTATE_AFTER {
            let i = self.order_index(slot);
            // DEV: `to_vec()` and `rotate_left(1)` become `rotate1`.
            let order = rotate1(&self.history[i].1);
            let from = slot + LAG;
            let ghost e = (from, order@);
            self.history.push((from, order));
            let ghost h1 = self.view();
            assert(h1 =~= h0.push(e));
            proof { lemma_push(h0, e, slot); }
            self.streak = 0;
            self.quiet_until = from;
            self.prune(slot);
            proof { assert(h1[0] == h0[0]); self.lemma_after_prune(h0, h1, slot); }
        } else {
            proof { lemma_same(h0, slot); }
        }
        self.fed = Ghost(Some(slot));
    }

    /// Applies membership change `key`, decided in `slot`: from `slot + LAG` on, `order`
    /// leads. Feed it in slot order, after `observe` for the same slot.
    pub fn reconfigure(&mut self, slot: u64, key: u64, order: Vec<u32>) -> (r: Option<(u64, u64)>)
        requires
            old(self).inv(), slot <= MAX_SLOT, order@.len() > 0,
            match old(self).fed@ { Some(f) => f <= slot, None => true },
        ensures
            final(self).inv(), final(self).fed@ == Some(slot),
            window_kept(old(self).view(), final(self).view(), slot),
    {
        let ghost h0 = self.view();
        proof { self.lemma_fed(slot); }
        // DEV: the epoch counter is not ported (`epoch + 1` would need a bound; it only
        // names the epoch).
        if settled_has(&self.settled, key) {
            self.fed = Ghost(Some(slot));
            proof { lemma_same(h0, slot); }
            return None;
        }
        self.settled.push(key);
        if slot < self.epoch_from {
            self.fed = Ghost(Some(slot));
            proof { lemma_same(h0, slot); }
            return None;
        }
        let from = slot + LAG;
        let ghost e = (from, order@);
        let n = self.history.len();
        // A rotation counted in this very slot is superseded.
        if self.history[n - 1].0 == from {
            assert(n >= 2) by {
                if n == 1 { assert(h0[0].0 <= slot); }
            }
            self.history.pop();
            self.history.push((from, order));
            assert(self.view() =~= h0.drop_last().push(e));
            proof { lemma_replace_last(h0, e, slot); }
        } else {
            self.history.push((from, order));
            assert(self.view() =~= h0.push(e));
            proof {
                assert forall|i: int| 0 <= i < h0.len() implies h0[i].0 < e.0 by {
                    assert(h0[i].0 <= h0[n - 1].0);
                }
                lemma_push(h0, e, slot);
            }
        }
        let ghost h1 = self.view();
        self.epoch_from = from;
        self.streak = 0;
        self.quiet_until = from;
        self.prune(slot);
        proof { assert(h1[0] == h0[0]); self.lemma_after_prune(h0, h1, slot); }
        self.fed = Ghost(Some(slot));
        Some((self.epoch, from))
    }

    /// Every entry starts at most at slot + LAG, the first at most at slot.
    proof fn lemma_fed(&self, slot: u64)
        requires self.inv(), match self.fed@ { Some(f) => f <= slot, None => true },
        ensures
            self.view()[0].0 <= slot,
            forall|i: int| 0 <= i < self.view().len() ==> (#[trigger] self.view()[i]).0 <= slot + LAG,
    {
        match self.fed@ {
            Some(f) => { assert(self.view()[0].0 <= f); assert(f <= slot); assert(self.view()[0].0 <= slot); }
            None => { assert(self.view()[0].0 == 0); assert(self.view()[0].0 <= slot); }
        }
    }

    /// After a push (h0 to h1) and a prune at `slot`, the window is kept from h0.
    proof fn lemma_after_prune(&self, h0: Seq<Entry>, h1: Seq<Entry>, slot: u64)
        requires
            well_formed(h1), well_formed(self.view()), window_kept(h0, h1, slot), slot <= MAX_SLOT,
            exists|d: int| 0 <= d < h1.len() && self.view() == h1.subrange(d, h1.len() as int)
                && in_force(h1, slot, d),
            forall|i: int| 0 <= i < h1.len() ==> (#[trigger] h1[i]).0 <= slot + LAG,
            h1[0].0 <= slot,
            self.streak < ROTATE_AFTER,
        ensures
            window_kept(h0, self.view(), slot),
            self.view()[0].0 <= slot,
            forall|i: int| 0 <= i < self.view().len() ==> (#[trigger] self.view()[i]).0 <= slot + LAG,
    {
        let d = choose|d: int| 0 <= d < h1.len() && self.view() == h1.subrange(d, h1.len() as int)
            && in_force(h1, slot, d);
        lemma_suffix(h1, d, slot);
        assert forall|k: u64| slot <= k < slot + LAG implies #[trigger] kept_at(h0, self.view(), k) by {
            assert(kept_at(h0, h1, k));
            lemma_in_force(self.view(), k);
            assert(kept_at(h1, self.view(), k));
            lemma_kept_trans(h0, h1, self.view(), k);
        }
        if d > 0 { assert(h1[d].0 <= slot); }
    }

    /// Older entries can no longer apply to any slot from `slot` on.
    fn prune(&mut self, slot: u64)
        requires well_formed(old(self).view()),
        ensures
            well_formed(final(self).view()),
            exists|d: int| 0 <= d < old(self).view().len()
                && final(self).view() == old(self).view().subrange(d, old(self).view().len() as int)
                && in_force(old(self).view(), slot, d),
            final(self).streak == old(self).streak, final(self).fed == old(self).fed,
            final(self).quiet_until == old(self).quiet_until,
            final(self).epoch == old(self).epoch, final(self).epoch_from == old(self).epoch_from,
            final(self).settled@ == old(self).settled@,
    {
        let keep_from = self.order_index(slot);
        // DEV: `drain(..keep_from)` keeps the tail with `split_off`.
        let tail = self.history.split_off(keep_from);
        self.history = tail;
        assert(self.view() =~= old(self).view().subrange(keep_from as int, old(self).view().len() as int));
    }
}


/// Keeping a slot's order is transitive.
proof fn lemma_kept_trans(a: Seq<Entry>, b: Seq<Entry>, c: Seq<Entry>, k: u64)
    requires well_formed(b), kept_at(a, b, k), kept_at(b, c, k),
    ensures kept_at(a, c, k),
{
    lemma_in_force(b, k);
    let jb = choose|j: int| in_force(b, k, j);
    assert forall|i: int, j: int| in_force(a, k, i) && in_force(c, k, j) implies a[i].1 == c[j].1 by {
        assert(a[i].1 == b[jb].1);
        assert(b[jb].1 == c[j].1);
    }
}

/// A new last entry starting at s + LAG keeps every slot below it.
proof fn lemma_push(h: Seq<Entry>, e: Entry, s: u64)
    requires
        well_formed(h), e.1.len() > 0, e.0 == s + LAG, s <= MAX_SLOT,
        forall|i: int| 0 <= i < h.len() ==> h[i].0 < e.0,
    ensures well_formed(h.push(e)), window_kept(h, h.push(e), s),
{
    let h2 = h.push(e);
    assert forall|i: int| 0 <= i < h2.len() implies (#[trigger] h2[i]).1.len() > 0 by {
        if i < h.len() { assert(h2[i] == h[i]); }
    }
    assert forall|k: u64| s <= k < s + LAG implies #[trigger] kept_at(h, h2, k) by {
        lemma_in_force(h, k);
        lemma_in_force(h2, k);
        let i0 = choose|i: int| in_force(h, k, i);
        assert(in_force(h2, k, i0));
    }
}

/// Replacing a last entry that starts at s + LAG keeps every slot below it.
proof fn lemma_replace_last(h: Seq<Entry>, e: Entry, s: u64)
    requires
        well_formed(h), h.len() >= 2, e.1.len() > 0, e.0 == s + LAG, s <= MAX_SLOT,
        h[h.len() - 1].0 == e.0,
    ensures
        well_formed(h.drop_last().push(e)), window_kept(h, h.drop_last().push(e), s),
{
    let h1 = h.drop_last();
    let h2 = h1.push(e);
    assert(well_formed(h1)) by {
        assert forall|i: int| 0 <= i < h1.len() implies (#[trigger] h1[i]).1.len() > 0 by {
            assert(h1[i] == h[i]);
        }
    }
    assert forall|i: int| 0 <= i < h1.len() implies h1[i].0 < e.0 by {
        assert(h[i].0 < h[h.len() - 1].0);
    }
    lemma_push(h1, e, s);
    assert forall|k: u64| s <= k < s + LAG implies #[trigger] kept_at(h, h2, k) by {
        lemma_in_force(h, k);
        lemma_in_force(h1, k);
        let i0 = choose|i: int| in_force(h1, k, i);
        assert(in_force(h, k, i0));
        assert(kept_at(h1, h2, k));
    }
}

/// The rotated order, first leader last. DEV: built by hand for `rotate_left(1)`.
fn rotate1(o: &Vec<u32>) -> (r: Vec<u32>)
    requires o@.len() > 0,
    ensures r@ == o@.subrange(1, o@.len() as int).push(o@[0]),
{
    let mut r = Vec::new();
    let mut j = 1;
    while j < o.len()
        invariant 1 <= j <= o@.len(), r@ == o@.subrange(1, j as int),
        decreases o@.len() - j,
    {
        r.push(o[j]);
        j = j + 1;
    }
    r.push(o[0]);
    r
}

/// Whether `key` was settled. DEV: a loop for `contains`.
fn settled_has(settled: &Vec<u64>, key: u64) -> (r: bool)
    ensures r == settled@.contains(key),
{
    let mut j = 0;
    while j < settled.len()
        invariant j <= settled@.len(), forall|x: int| 0 <= x < j ==> settled@[x] != key,
        decreases settled@.len() - j,
    {
        if settled[j] == key {
            return true;
        }
        j = j + 1;
    }
    false
}

/// An unchanged history keeps every slot.
proof fn lemma_same(h: Seq<Entry>, s: u64)
    requires well_formed(h),
    ensures window_kept(h, h, s),
{
    assert forall|k: u64| s <= k < s + LAG implies #[trigger] kept_at(h, h, k) by {
        lemma_in_force(h, k);
    }
}

/// Dropping entries before the one in force at `s` keeps every slot from `s` on.
proof fn lemma_suffix(h: Seq<Entry>, d: int, s: u64)
    requires well_formed(h), 0 <= d < h.len(), in_force(h, s, d),
    ensures
        well_formed(h.subrange(d, h.len() as int)),
        forall|k: u64, i: int, j: int| k >= s && in_force(h, k, i)
            && in_force(h.subrange(d, h.len() as int), k, j) ==> h[i].1 == h.subrange(d, h.len() as int)[j].1,
{
    let t = h.subrange(d, h.len() as int);
    assert forall|k: u64, i: int, j: int| k >= s && in_force(h, k, i) && in_force(t, k, j)
        implies h[i].1 == t[j].1 by {
        lemma_in_force(h, k);
        assert(in_force(h, k, j + d)) by {
            if j == 0 && d > 0 { assert(h[d].0 <= s); }
        }
    }
}

} // verus!
