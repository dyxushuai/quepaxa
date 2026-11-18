// crates/quepaxa/src/replica/entries.rs and entries/back.rs (`Entries`) ported to Verus
// (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic statement by statement, except where marked DEV.
// Entries are small keys (origin, incarnation, id) with a membership-change flag; a batch's
// value is its entries; the dedup state is an input set of keys (dedup.rs is proved apart).
// A ghost copy of the dedup state of the last call ties the calls together.
//
// Proved, for every sequence of calls (own ids fresh, at most 2^32 entries held at a call):
// E1 every own entry is in the place its `at` names (forwarded with that time); unless it is
//    marked `copied`, that place holds exactly one copy and no other place but a batch in a
//    slot holds one;
// E2 own records leave only through `resolved`, `installed_snapshot` and `fence`, which
//    return them, and none is awaited once committed (`settled`);
// E3 every entry held stays held, is committed (in dedup), or is handed out (`route`);
// E4 the counters (`unslotted`, `foreign`, `changes`) match, and a batch out of a slot with
//    nothing left to commit is due to be pruned;
// E5 after `route` nothing is pending unless the replica proposes directly;
// `proposable` is true exactly when a batch waits for a slot or `cut` has entries to take;
// a lost slot hands back only entries not in dedup, and, for a batch handed on when stepping
// aside, no other member's entry (#187 D2); losing the lead keeps every batch in a slot;
// stepping aside leaves nothing relayed, no batch waiting for a slot, and every batch handed.
//
// Departures from the original's code, forced by what vstd specifies, are marked DEV;
// VERIFICATION.md lists them. When entries.rs or entries/back.rs changes, port the change here.
use vstd::prelude::*;

verus! {

#[derive(Clone, Copy)]
pub struct Item { pub origin: u64, pub inc: u64, pub id: u64, pub change: bool }

pub open spec fn key(x: Item) -> (u64, u64, u64) { (x.origin, x.inc, x.id) }

#[derive(Clone, Copy)]
pub enum At { Pending, Relayed, Forwarded { since: u64 }, Batched { seq: u64 } }

/// DEV: the record carries its id and whether it is a change (`kind`); `since` is left out.
#[derive(Clone, Copy)]
pub struct Own { pub id: u64, pub change: bool, pub hedged: bool, pub at: At, pub copied: bool }

/// DEV: the value is the entries. `handed`: its entries were handed on when stepping aside.
pub struct Batch { pub seq: u64, pub entries: Vec<Item>, pub slot: Option<u64>, pub changes: u64, pub handed: bool }

pub struct BatchV { pub seq: u64, pub entries: Seq<Item>, pub slot: Option<u64> }

pub open spec fn bv(b: Batch) -> BatchV { BatchV { seq: b.seq, entries: b.entries@, slot: b.slot } }

pub open spec fn views(bs: Seq<Batch>) -> Seq<BatchV> { Seq::new(bs.len(), |i: int| bv(bs[i])) }

// ---- counting ----

pub open spec fn count(s: Seq<Item>, p: spec_fn(Item) -> bool) -> nat
    decreases s.len(),
{
    if s.len() == 0 { 0 } else { count(s.drop_last(), p) + if p(s.last()) { 1nat } else { 0nat } }
}

pub open spec fn bsum(bs: Seq<BatchV>, p: spec_fn(Item) -> bool) -> nat
    decreases bs.len(),
{
    if bs.len() == 0 { 0 } else { bsum(bs.drop_last(), p) + count(bs.last().entries, p) }
}

pub open spec fn this(me: u64, inc: u64, id: u64) -> spec_fn(Item) -> bool {
    |x: Item| x.origin == me && x.inc == inc && x.id == id
}

pub proof fn lemma_count_add(a: Seq<Item>, b: Seq<Item>, p: spec_fn(Item) -> bool)
    ensures count(a + b, p) == count(a, p) + count(b, p),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(a + b =~= a);
    } else {
        lemma_count_add(a, b.drop_last(), p);
        assert((a + b).drop_last() =~= a + b.drop_last());
    }
}

pub proof fn lemma_count_push(s: Seq<Item>, x: Item)
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] count(s.push(x), p) == count(s, p) + if p(x) { 1nat } else { 0nat },
{
    assert(s.push(x).drop_last() =~= s);
}

pub proof fn lemma_count_one(x: Item, p: spec_fn(Item) -> bool)
    ensures count(seq![x], p) == if p(x) { 1nat } else { 0nat },
{
    assert(seq![x].drop_last() =~= Seq::<Item>::empty());
    assert(count(Seq::<Item>::empty(), p) == 0);
}

pub proof fn lemma_count_pop(s: Seq<Item>)
    requires s.len() > 0,
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] count(s, p) == count(s.subrange(1, s.len() as int), p) + if p(s[0]) { 1nat } else { 0nat },
{
    assert forall|p: spec_fn(Item) -> bool| #[trigger] count(s, p) == count(s.subrange(1, s.len() as int), p) + if p(s[0]) { 1nat } else { 0nat } by {
        lemma_count_add(seq![s[0]], s.subrange(1, s.len() as int), p);
        assert(seq![s[0]] + s.subrange(1, s.len() as int) =~= s);
        lemma_count_one(s[0], p);
    }
}

pub proof fn lemma_count_front(s: Seq<Item>, x: Item)
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] count(s.insert(0, x), p) == count(s, p) + if p(x) { 1nat } else { 0nat },
{
    assert forall|p: spec_fn(Item) -> bool| #[trigger] count(s.insert(0, x), p) == count(s, p) + if p(x) { 1nat } else { 0nat } by {
        lemma_count_add(seq![x], s, p);
        assert(seq![x] + s =~= s.insert(0, x));
        lemma_count_one(x, p);
    }
}

pub proof fn lemma_count_pos(s: Seq<Item>, p: spec_fn(Item) -> bool)
    ensures count(s, p) > 0 <==> exists|i: int| 0 <= i < s.len() && p(#[trigger] s[i]),
    decreases s.len(),
{
    if s.len() > 0 {
        lemma_count_pos(s.drop_last(), p);
        if count(s.drop_last(), p) > 0 {
            let i = choose|i: int| 0 <= i < s.drop_last().len() && p(#[trigger] s.drop_last()[i]);
            assert(s[i] == s.drop_last()[i]);
        }
        if exists|i: int| 0 <= i < s.len() && p(#[trigger] s[i]) {
            let i = choose|i: int| 0 <= i < s.len() && p(#[trigger] s[i]);
            if i < s.len() - 1 {
                assert(s.drop_last()[i] == s[i]);
            }
        }
    }
}

pub proof fn lemma_count_le(s: Seq<Item>, p: spec_fn(Item) -> bool)
    ensures count(s, p) <= s.len(),
    decreases s.len(),
{
    if s.len() > 0 { lemma_count_le(s.drop_last(), p); }
}

pub proof fn lemma_count_none(s: Seq<Item>, x: spec_fn(Item) -> bool)
    requires forall|i: int| 0 <= i < s.len() ==> !x(#[trigger] s[i]),
    ensures count(s, x) == 0,
{
    lemma_count_pos(s, x);
}

pub proof fn lemma_bsum_add(a: Seq<BatchV>, b: Seq<BatchV>, p: spec_fn(Item) -> bool)
    ensures bsum(a + b, p) == bsum(a, p) + bsum(b, p),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(a + b =~= a);
    } else {
        lemma_bsum_add(a, b.drop_last(), p);
        assert((a + b).drop_last() =~= a + b.drop_last());
    }
}

pub proof fn lemma_bsum_push(bs: Seq<BatchV>, b: BatchV)
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] bsum(bs.push(b), p) == bsum(bs, p) + count(b.entries, p),
{
    assert(bs.push(b).drop_last() =~= bs);
}

pub proof fn lemma_bsum_remove(bs: Seq<BatchV>, j: int)
    requires 0 <= j < bs.len(),
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] bsum(bs, p) == bsum(bs.remove(j), p) + count(bs[j].entries, p),
{
    assert forall|p: spec_fn(Item) -> bool| #[trigger] bsum(bs, p) == bsum(bs.remove(j), p) + count(bs[j].entries, p) by {
        let (a, c) = (bs.subrange(0, j), bs.subrange(j + 1, bs.len() as int));
        assert(bs =~= a + seq![bs[j]] + c);
        assert(bs.remove(j) =~= a + c);
        lemma_bsum_add(a + seq![bs[j]], c, p);
        lemma_bsum_add(a, seq![bs[j]], p);
        lemma_bsum_add(a, c, p);
        assert(seq![bs[j]].drop_last() =~= Seq::<BatchV>::empty());
        assert(bsum(Seq::<BatchV>::empty(), p) == 0);
        assert(bsum(seq![bs[j]], p) == bsum(Seq::<BatchV>::empty(), p) + count(bs[j].entries, p));
    }
}

/// Changing a batch's slot changes no sum.
pub proof fn lemma_bsum_update(bs: Seq<BatchV>, j: int, b: BatchV)
    requires 0 <= j < bs.len(), b.entries == bs[j].entries,
    ensures forall|p: spec_fn(Item) -> bool| #[trigger] bsum(bs.update(j, b), p) == bsum(bs, p),
{
    lemma_bsum_remove(bs, j);
    lemma_bsum_remove(bs.update(j, b), j);
    assert(bs.update(j, b).remove(j) =~= bs.remove(j));
}

pub proof fn lemma_count_le_any(s: Seq<Item>, p: spec_fn(Item) -> bool)
    ensures count(s, p) <= count(s, |x: Item| true), count(s, |x: Item| true) == s.len(),
    decreases s.len(),
{
    if s.len() > 0 { lemma_count_le_any(s.drop_last(), p); }
}

pub proof fn lemma_bsum_le(bs: Seq<BatchV>, p: spec_fn(Item) -> bool)
    ensures bsum(bs, p) <= bsum(bs, |x: Item| true),
    decreases bs.len(),
{
    if bs.len() > 0 {
        lemma_bsum_le(bs.drop_last(), p);
        lemma_count_le_any(bs.last().entries, p);
    }
}

pub open spec fn ocount(os: Seq<Own>) -> nat
    decreases os.len(),
{
    if os.len() == 0 { 0 } else { ocount(os.drop_last()) + if os.last().change { 1nat } else { 0nat } }
}

pub proof fn lemma_ocount_push(os: Seq<Own>, o: Own)
    ensures ocount(os.push(o)) == ocount(os) + if o.change { 1nat } else { 0nat },
{
    assert(os.push(o).drop_last() =~= os);
}

pub proof fn lemma_ocount_add(a: Seq<Own>, b: Seq<Own>)
    ensures ocount(a + b) == ocount(a) + ocount(b),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(a + b =~= a);
    } else {
        lemma_ocount_add(a, b.drop_last());
        assert((a + b).drop_last() =~= a + b.drop_last());
    }
}

pub proof fn lemma_ocount_remove(os: Seq<Own>, k: int)
    requires 0 <= k < os.len(),
    ensures ocount(os) == ocount(os.remove(k)) + if os[k].change { 1nat } else { 0nat },
{
    let (a, c) = (os.subrange(0, k), os.subrange(k + 1, os.len() as int));
    assert(os =~= a + seq![os[k]] + c);
    assert(os.remove(k) =~= a + c);
    lemma_ocount_add(a + seq![os[k]], c);
    lemma_ocount_add(a, seq![os[k]]);
    lemma_ocount_add(a, c);
    assert(seq![os[k]].drop_last() =~= Seq::<Own>::empty());
    assert(ocount(Seq::<Own>::empty()) == 0);
    assert(ocount(seq![os[k]]) == ocount(Seq::<Own>::empty()) + if os[k].change { 1nat } else { 0nat });
}

pub proof fn lemma_ocount_update(os: Seq<Own>, k: int, o: Own)
    requires 0 <= k < os.len(), o.change == os[k].change,
    ensures ocount(os.update(k, o)) == ocount(os),
{
    lemma_ocount_remove(os, k);
    lemma_ocount_remove(os.update(k, o), k);
    assert(os.update(k, o).remove(k) =~= os.remove(k));
}

pub proof fn lemma_ocount_le(os: Seq<Own>)
    ensures ocount(os) <= os.len(),
    decreases os.len(),
{
    if os.len() > 0 { lemma_ocount_le(os.drop_last()); }
}

/// The same changes among own records with the same kinds.
pub proof fn lemma_ocount_same(a: Seq<Own>, b: Seq<Own>)
    requires a.len() == b.len(), forall|k: int| 0 <= k < a.len() ==> (#[trigger] a[k]).change == b[k].change,
    ensures ocount(a) == ocount(b),
    decreases a.len(),
{
    if a.len() > 0 {
        lemma_ocount_same(a.drop_last(), b.drop_last());
        assert(a.last().change == b[b.len() - 1].change);
    }
}

pub open spec fn foreign(me: u64) -> spec_fn(Item) -> bool { |x: Item| x.origin != me }

pub open spec fn change() -> spec_fn(Item) -> bool { |x: Item| x.change }

pub open spec fn every() -> spec_fn(Item) -> bool { |x: Item| true }

// ---- where the copies of an own entry are ----

/// The queues, forwarded entries, batches and own records, as values: the state proper, or
/// one with entries in hand counted where they came from or go.
pub struct V {
    pub pending: Seq<Item>,
    pub relayed: Seq<Item>,
    pub fwd: Seq<(Item, u64)>,
    pub batches: Seq<BatchV>,
    pub own: Seq<Own>,
}

/// The batches, by seq: whether in a slot, and how many copies of the entries `x` matches.
pub open spec fn bmap(bs: Seq<BatchV>, x: spec_fn(Item) -> bool) -> Map<u64, (bool, nat)>
    decreases bs.len(),
{
    if bs.len() == 0 {
        Map::empty()
    } else {
        bmap(bs.drop_last(), x).insert(bs.last().seq, (bs.last().slot is Some, count(bs.last().entries, x)))
    }
}

pub open spec fn unique_seqs(bs: Seq<BatchV>) -> bool {
    forall|i: int, j: int| 0 <= i < j < bs.len() ==> bs[i].seq != bs[j].seq
}

pub proof fn lemma_bmap(bs: Seq<BatchV>, x: spec_fn(Item) -> bool)
    requires unique_seqs(bs),
    ensures
        forall|j: int| 0 <= j < bs.len() ==> #[trigger] bmap(bs, x).contains_key(bs[j].seq)
            && bmap(bs, x)[bs[j].seq] == (bs[j].slot is Some, count(bs[j].entries, x)),
        forall|s: u64| #[trigger] bmap(bs, x).contains_key(s) ==> exists|j: int| 0 <= j < bs.len() && bs[j].seq == s,
    decreases bs.len(),
{
    if bs.len() > 0 {
        let d = bs.drop_last();
        assert(unique_seqs(d)) by {
            assert forall|i: int, j: int| 0 <= i < j < d.len() implies d[i].seq != d[j].seq by {
                assert(d[i] == bs[i] && d[j] == bs[j]);
            }
        }
        lemma_bmap(d, x);
        assert(bmap(bs, x) == bmap(d, x).insert(bs.last().seq, (bs.last().slot is Some, count(bs.last().entries, x))));
        assert forall|j: int| 0 <= j < bs.len() implies #[trigger] bmap(bs, x).contains_key(bs[j].seq)
            && bmap(bs, x)[bs[j].seq] == (bs[j].slot is Some, count(bs[j].entries, x)) by {
            if j < bs.len() - 1 {
                assert(d[j] == bs[j]);
                assert(bs[j].seq != bs.last().seq);
            }
        }
        assert forall|s: u64| #[trigger] bmap(bs, x).contains_key(s) implies exists|j: int| 0 <= j < bs.len() && bs[j].seq == s by {
            if s != bs.last().seq {
                assert(bmap(d, x).contains_key(s));
                let j = choose|j: int| 0 <= j < d.len() && d[j].seq == s;
                assert(bs[j] == d[j]);
            } else {
                assert(bs[bs.len() - 1].seq == s);
            }
        }
    }
}

/// Own forwarded entries by id, with when.
pub open spec fn fmap(f: Seq<(Item, u64)>) -> Map<u64, u64>
    decreases f.len(),
{
    if f.len() == 0 { Map::empty() } else { fmap(f.drop_last()).insert(f.last().0.id, f.last().1) }
}

pub open spec fn unique_ids(f: Seq<(Item, u64)>) -> bool {
    forall|i: int, j: int| 0 <= i < j < f.len() ==> f[i].0.id != f[j].0.id
}

pub proof fn lemma_fmap(f: Seq<(Item, u64)>)
    requires unique_ids(f),
    ensures
        forall|j: int| 0 <= j < f.len() ==> #[trigger] fmap(f).contains_key(f[j].0.id) && fmap(f)[f[j].0.id] == f[j].1,
        forall|id: u64| #[trigger] fmap(f).contains_key(id) ==> exists|j: int| 0 <= j < f.len() && f[j].0.id == id,
    decreases f.len(),
{
    if f.len() > 0 {
        let d = f.drop_last();
        assert(unique_ids(d)) by {
            assert forall|i: int, j: int| 0 <= i < j < d.len() implies d[i].0.id != d[j].0.id by {
                assert(d[i] == f[i] && d[j] == f[j]);
            }
        }
        lemma_fmap(d);
        assert(fmap(f) == fmap(d).insert(f.last().0.id, f.last().1));
        assert forall|j: int| 0 <= j < f.len() implies #[trigger] fmap(f).contains_key(f[j].0.id) && fmap(f)[f[j].0.id] == f[j].1 by {
            if j < f.len() - 1 {
                assert(d[j] == f[j]);
                assert(f[j].0.id != f.last().0.id);
            }
        }
        assert forall|id: u64| #[trigger] fmap(f).contains_key(id) implies exists|j: int| 0 <= j < f.len() && f[j].0.id == id by {
            if id != f.last().0.id {
                assert(fmap(d).contains_key(id));
                let j = choose|j: int| 0 <= j < d.len() && d[j].0.id == id;
                assert(f[j] == d[j]);
            } else {
                assert(f[f.len() - 1].0.id == id);
            }
        }
    }
}

/// Where the copies of own entry `id` are.
pub struct Census {
    pub p: nat,
    pub r: nat,
    pub f: Option<u64>,
    pub b: Map<u64, (bool, nat)>,
}

pub open spec fn census(v: V, me: u64, inc: u64, id: u64) -> Census {
    let x = this(me, inc, id);
    let fm = fmap(v.fwd);
    Census {
        p: count(v.pending, x),
        r: count(v.relayed, x),
        f: if fm.contains_key(id) { Some(fm[id]) } else { None },
        b: bmap(v.batches, x),
    }
}

/// E1, first half: the place `at` names holds the entry.
pub open spec fn placed(c: Census, at: At) -> bool {
    match at {
        At::Pending => c.p > 0,
        At::Relayed => c.r > 0,
        At::Forwarded { since } => c.f == Some(since),
        At::Batched { seq } => c.b.contains_key(seq) && c.b[seq].1 > 0,
    }
}

/// E1, second half: one copy at `at`, and none elsewhere but in batches in a slot.
pub open spec fn alone(c: Census, at: At) -> bool {
    &&& c.p == if at is Pending { 1nat } else { 0nat }
    &&& c.r == if at is Relayed { 1nat } else { 0nat }
    &&& c.f is Some ==> at is Forwarded
    &&& forall|s: u64| #[trigger] c.b.contains_key(s) && (!c.b[s].0 || at == (At::Batched { seq: s })) ==>
            c.b[s].1 == if at == (At::Batched { seq: s }) { 1nat } else { 0nat }
}

pub open spec fn fine(v: V, me: u64, inc: u64, o: Own) -> bool {
    let c = census(v, me, inc, o.id);
    placed(c, o.at) && (!o.copied ==> alone(c, o.at))
}

/// E1: every own entry is in the place `at` names; unless `copied`, that place holds one
/// copy and no other place but a batch in a slot holds one.
pub open spec fn e1(v: V, me: u64, inc: u64) -> bool {
    forall|k: int| 0 <= k < v.own.len() ==> fine(v, me, inc, #[trigger] v.own[k])
}

pub open spec fn unique_own(os: Seq<Own>) -> bool {
    forall|i: int, j: int| 0 <= i < j < os.len() ==> os[i].id != os[j].id
}

/// One step: the census of every id but `id` is unchanged, and own records change at most
/// for `id`, which is then fine.
pub proof fn lemma_e1_step(v: V, w: V, me: u64, inc: u64, id: u64)
    requires
        e1(v, me, inc),
        w.own.len() == v.own.len(),
        forall|k: int| 0 <= k < v.own.len() && (#[trigger] v.own[k]).id != id ==> w.own[k] == v.own[k],
        forall|k: int| 0 <= k < v.own.len() && (#[trigger] v.own[k]).id == id ==> w.own[k].id == id && fine(w, me, inc, w.own[k]),
        forall|i: u64| i != id ==> #[trigger] census(w, me, inc, i) == census(v, me, inc, i),
    ensures e1(w, me, inc),
{
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        if v.own[k].id != id {
            assert(fine(v, me, inc, v.own[k]));
        }
    }
}

pub open spec fn bentry(b: BatchV, x: spec_fn(Item) -> bool) -> (bool, nat) {
    (b.slot is Some, count(b.entries, x))
}

pub proof fn lemma_bmap_update(bs: Seq<BatchV>, j: int, b: BatchV)
    requires unique_seqs(bs), 0 <= j < bs.len(), b.seq == bs[j].seq,
    ensures
        unique_seqs(bs.update(j, b)),
        forall|x: spec_fn(Item) -> bool| #[trigger] bmap(bs.update(j, b), x) == bmap(bs, x).insert(b.seq, bentry(b, x)),
{
    let w = bs.update(j, b);
    assert(unique_seqs(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].seq != w[c].seq by {
            assert(w[a].seq == bs[a].seq && w[c].seq == bs[c].seq);
        }
    }
    assert forall|x: spec_fn(Item) -> bool| #[trigger] bmap(w, x) == bmap(bs, x).insert(b.seq, bentry(b, x)) by {
        lemma_bmap(bs, x);
        lemma_bmap(w, x);
        let m = bmap(bs, x).insert(b.seq, bentry(b, x));
        assert forall|s: u64| bmap(w, x).contains_key(s) <==> m.contains_key(s) by {
            if bmap(w, x).contains_key(s) {
                let i = choose|i: int| 0 <= i < w.len() && w[i].seq == s;
                if i != j { assert(bmap(bs, x).contains_key(bs[i].seq)); }
            }
            if bmap(bs, x).contains_key(s) {
                let i = choose|i: int| 0 <= i < bs.len() && bs[i].seq == s;
                assert(bmap(w, x).contains_key(w[i].seq));
            }
            if s == b.seq { assert(bmap(w, x).contains_key(w[j].seq)); }
        }
        assert forall|s: u64| #[trigger] bmap(w, x).contains_key(s) implies bmap(w, x)[s] == m[s] by {
            let i = choose|i: int| 0 <= i < w.len() && w[i].seq == s;
            if i != j {
                assert(bmap(bs, x).contains_key(bs[i].seq));
                assert(w[i] == bs[i]);
            } else {
                assert(w[i] == b);
            }
        }
        assert(bmap(w, x) =~= m);
    }
}

pub proof fn lemma_bmap_push(bs: Seq<BatchV>, b: BatchV)
    requires unique_seqs(bs), forall|i: int| 0 <= i < bs.len() ==> (#[trigger] bs[i]).seq != b.seq,
    ensures
        unique_seqs(bs.push(b)),
        forall|x: spec_fn(Item) -> bool| #[trigger] bmap(bs.push(b), x) == bmap(bs, x).insert(b.seq, bentry(b, x)),
{
    let w = bs.push(b);
    assert(unique_seqs(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].seq != w[c].seq by {
            if c == bs.len() { assert(bs[a].seq != b.seq); } else { assert(w[a] == bs[a] && w[c] == bs[c]); }
        }
    }
    assert forall|x: spec_fn(Item) -> bool| #[trigger] bmap(bs.push(b), x) == bmap(bs, x).insert(b.seq, bentry(b, x)) by {
        assert(w.drop_last() =~= bs);
    }
}

pub proof fn lemma_bmap_remove(bs: Seq<BatchV>, j: int)
    requires unique_seqs(bs), 0 <= j < bs.len(),
    ensures
        unique_seqs(bs.remove(j)),
        forall|x: spec_fn(Item) -> bool| #[trigger] bmap(bs.remove(j), x) == bmap(bs, x).remove(bs[j].seq),
{
    let w = bs.remove(j);
    assert(unique_seqs(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].seq != w[c].seq by {
            let a2 = if a < j { a } else { a + 1 };
            let c2 = if c < j { c } else { c + 1 };
            assert(w[a] == bs[a2] && w[c] == bs[c2]);
        }
    }
    assert forall|x: spec_fn(Item) -> bool| #[trigger] bmap(w, x) == bmap(bs, x).remove(bs[j].seq) by {
        lemma_bmap(bs, x);
        lemma_bmap(w, x);
        let m = bmap(bs, x).remove(bs[j].seq);
        assert forall|s: u64| bmap(w, x).contains_key(s) <==> m.contains_key(s) by {
            if bmap(w, x).contains_key(s) {
                let i = choose|i: int| 0 <= i < w.len() && w[i].seq == s;
                let i2 = if i < j { i } else { i + 1 };
                assert(w[i] == bs[i2]);
                assert(bmap(bs, x).contains_key(bs[i2].seq));
            }
            if m.contains_key(s) {
                let i = choose|i: int| 0 <= i < bs.len() && bs[i].seq == s;
                assert(i != j);
                let i2 = if i < j { i } else { i - 1 };
                assert(w[i2] == bs[i]);
                assert(bmap(w, x).contains_key(w[i2].seq));
            }
        }
        assert forall|s: u64| #[trigger] bmap(w, x).contains_key(s) implies bmap(w, x)[s] == m[s] by {
            let i = choose|i: int| 0 <= i < w.len() && w[i].seq == s;
            let i2 = if i < j { i } else { i + 1 };
            assert(w[i] == bs[i2]);
            assert(bmap(w, x).contains_key(w[i].seq));
            assert(bmap(bs, x).contains_key(bs[i2].seq));
            assert(bs[i2].seq != bs[j].seq);
        }
        assert(bmap(w, x) =~= m);
    }
}

pub proof fn lemma_fmap_set(f: Seq<(Item, u64)>, j: int, e: (Item, u64))
    requires unique_ids(f), 0 <= j < f.len(), e.0.id == f[j].0.id,
    ensures unique_ids(f.update(j, e)), fmap(f.update(j, e)) == fmap(f).insert(e.0.id, e.1),
{
    let w = f.update(j, e);
    assert(unique_ids(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].0.id != w[c].0.id by {
            assert(w[a].0.id == f[a].0.id && w[c].0.id == f[c].0.id);
        }
    }
    lemma_fmap(f);
    lemma_fmap(w);
    let m = fmap(f).insert(e.0.id, e.1);
    assert forall|id: u64| fmap(w).contains_key(id) <==> m.contains_key(id) by {
        if fmap(w).contains_key(id) {
            let i = choose|i: int| 0 <= i < w.len() && w[i].0.id == id;
            if i != j { assert(fmap(f).contains_key(f[i].0.id)); }
        }
        if fmap(f).contains_key(id) {
            let i = choose|i: int| 0 <= i < f.len() && f[i].0.id == id;
            assert(fmap(w).contains_key(w[i].0.id));
        }
        if id == e.0.id { assert(fmap(w).contains_key(w[j].0.id)); }
    }
    assert forall|id: u64| #[trigger] fmap(w).contains_key(id) implies fmap(w)[id] == m[id] by {
        let i = choose|i: int| 0 <= i < w.len() && w[i].0.id == id;
        assert(fmap(w).contains_key(w[i].0.id));
        if i != j {
            assert(w[i] == f[i]);
            assert(fmap(f).contains_key(f[i].0.id));
        }
    }
    assert(fmap(w) =~= m);
}

pub proof fn lemma_fmap_push(f: Seq<(Item, u64)>, e: (Item, u64))
    requires unique_ids(f), forall|i: int| 0 <= i < f.len() ==> (#[trigger] f[i]).0.id != e.0.id,
    ensures unique_ids(f.push(e)), fmap(f.push(e)) == fmap(f).insert(e.0.id, e.1),
{
    let w = f.push(e);
    assert(unique_ids(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].0.id != w[c].0.id by {
            if c == f.len() { assert(f[a].0.id != e.0.id); } else { assert(w[a] == f[a] && w[c] == f[c]); }
        }
    }
    assert(w.drop_last() =~= f);
}

pub proof fn lemma_fmap_remove(f: Seq<(Item, u64)>, j: int)
    requires unique_ids(f), 0 <= j < f.len(),
    ensures unique_ids(f.remove(j)), fmap(f.remove(j)) == fmap(f).remove(f[j].0.id),
{
    let w = f.remove(j);
    assert(unique_ids(w)) by {
        assert forall|a: int, c: int| 0 <= a < c < w.len() implies w[a].0.id != w[c].0.id by {
            let a2 = if a < j { a } else { a + 1 };
            let c2 = if c < j { c } else { c + 1 };
            assert(w[a] == f[a2] && w[c] == f[c2]);
        }
    }
    lemma_fmap(f);
    lemma_fmap(w);
    let m = fmap(f).remove(f[j].0.id);
    assert forall|id: u64| fmap(w).contains_key(id) <==> m.contains_key(id) by {
        if fmap(w).contains_key(id) {
            let i = choose|i: int| 0 <= i < w.len() && w[i].0.id == id;
            let i2 = if i < j { i } else { i + 1 };
            assert(w[i] == f[i2]);
            assert(fmap(f).contains_key(f[i2].0.id));
        }
        if m.contains_key(id) {
            let i = choose|i: int| 0 <= i < f.len() && f[i].0.id == id;
            assert(i != j);
            let i2 = if i < j { i } else { i - 1 };
            assert(w[i2] == f[i]);
            assert(fmap(w).contains_key(w[i2].0.id));
        }
    }
    assert forall|id: u64| #[trigger] fmap(w).contains_key(id) implies fmap(w)[id] == m[id] by {
        let i = choose|i: int| 0 <= i < w.len() && w[i].0.id == id;
        let i2 = if i < j { i } else { i + 1 };
        assert(w[i] == f[i2]);
        assert(fmap(w).contains_key(w[i].0.id));
        assert(fmap(f).contains_key(f[i2].0.id));
        assert(f[i2].0.id != f[j].0.id);
    }
    assert(fmap(w) =~= m);
}

/// Giving a batch a slot keeps E1: copies in a slotted batch are allowed.
pub proof fn lemma_e1_slotted(v: V, me: u64, inc: u64, j: int, slot: u64)
    requires e1(v, me, inc), unique_seqs(v.batches), 0 <= j < v.batches.len(),
    ensures ({
        let b = v.batches[j];
        e1(V { batches: v.batches.update(j, BatchV { seq: b.seq, entries: b.entries, slot: Some(slot) }), ..v }, me, inc)
    }),
{
    let b = v.batches[j];
    let nb = BatchV { seq: b.seq, entries: b.entries, slot: Some(slot) };
    let w = V { batches: v.batches.update(j, nb), ..v };
    lemma_bmap_update(v.batches, j, nb);
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        let o = w.own[k];
        let x = this(me, inc, o.id);
        assert(fine(v, me, inc, o));
        lemma_bmap(v.batches, x);
        let (c, d) = (census(v, me, inc, o.id), census(w, me, inc, o.id));
        assert(d.b == c.b.insert(b.seq, (true, count(b.entries, x))));
        assert(bmap(v.batches, x).contains_key(v.batches[j].seq));
        assert(c.b.contains_key(b.seq) && c.b[b.seq].1 == count(b.entries, x));
        if !o.copied {
            assert forall|s: u64| #[trigger] d.b.contains_key(s) && (!d.b[s].0 || o.at == (At::Batched { seq: s })) implies
                d.b[s].1 == if o.at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                assert(c.b.contains_key(s));
            }
        }
    }
}

// ---- the state ----

/// Ids are keys: of own records, of forwarded entries (all own), of batches.
pub open spec fn shape(v: V, me: u64, inc: u64) -> bool {
    &&& unique_own(v.own)
    &&& unique_ids(v.fwd)
    &&& forall|j: int| 0 <= j < v.fwd.len() ==> (#[trigger] v.fwd[j]).0.origin == me && v.fwd[j].0.inc == inc
    &&& unique_seqs(v.batches)
}

/// An entry with key `k` is somewhere here (E3).
pub open spec fn holds(v: V, k: (u64, u64, u64)) -> bool {
    ||| exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k
    ||| exists|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k
    ||| exists|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k
    ||| exists|j: int, i: int| 0 <= j < v.batches.len() && 0 <= i < v.batches[j].entries.len() && key(#[trigger] v.batches[j].entries[i]) == k
}

/// Some forwarded entry has key `k`.
pub open spec fn fwd_has(f: Seq<(Item, u64)>, k: (u64, u64, u64)) -> bool {
    exists|i: int| 0 <= i < f.len() && key((#[trigger] f[i]).0) == k
}

pub open spec fn has_own(v: V, id: u64) -> bool {
    exists|k: int| 0 <= k < v.own.len() && (#[trigger] v.own[k]).id == id
}

/// At most this many entries are held at a call, so the counters never overflow.
pub open spec fn held_max() -> nat { 0x1_0000_0000 }

pub open spec fn held(v: V) -> nat {
    v.pending.len() + v.relayed.len() + v.fwd.len() + bsum(v.batches, every()) + v.own.len()
}

/// The dedup state, as the keys it covers. DEV: an input set (dedup.rs is proved apart).
pub struct Dedup {
    pub keys: Vec<(u64, u64, u64)>,
}

impl Dedup {
    pub open spec fn has(self, k: (u64, u64, u64)) -> bool { self.keys@.contains(k) }

    pub fn contains(&self, origin: u64, inc: u64, id: u64) -> (r: bool)
        ensures r == self.has((origin, inc, id)),
    {
        let mut i = 0;
        while i < self.keys.len()
            invariant i <= self.keys.len(), forall|j: int| 0 <= j < i ==> self.keys@[j] != (origin, inc, id),
            decreases self.keys.len() - i,
        {
            if self.keys[i].0 == origin && self.keys[i].1 == inc && self.keys[i].2 == id {
                assert(self.keys@[i as int] == (origin, inc, id));
                return true;
            }
            i += 1;
        }
        false
    }
}

/// Batch `s` has entries and no slot.
pub open spec fn waits(bs: Seq<Batch>, s: u64) -> bool {
    exists|j: int| 0 <= j < bs.len() && (#[trigger] bs[j]).seq == s && bs[j].slot is None && bs[j].entries@.len() > 0
}

pub struct Entries {
    pub me: u64,
    pub inc: u64,
    pub pending: Vec<Item>,
    pub relayed: Vec<Item>,
    // DEV: vectors keyed by id or seq for the `BTreeMap`s and the `BTreeSet`.
    pub forwarded: Vec<(Item, u64)>,
    pub batches: Vec<Batch>,
    pub own: Vec<Own>,
    pub next_batch: u64,
    pub unslotted: Vec<u64>,
    pub foreign: u64,
    pub changes: u64,
    pub prune_due: bool,
    /// The dedup state of the last call.
    pub dedup: Ghost<Set<(u64, u64, u64)>>,
}

impl Entries {
    pub open spec fn v(self) -> V {
        V { pending: self.pending@, relayed: self.relayed@, fwd: self.forwarded@, batches: views(self.batches@), own: self.own@ }
    }

    /// E4: the counters match.
    pub open spec fn counted(self) -> bool {
        let v = self.v();
        &&& self.foreign as int == count(v.relayed, foreign(self.me)) + bsum(v.batches, foreign(self.me))
        &&& self.changes as int == ocount(v.own) + count(v.pending, change()) + count(v.relayed, change()) + bsum(v.batches, change())
        &&& forall|j: int| 0 <= j < self.batches@.len() ==> (#[trigger] self.batches@[j]).changes as int == count(self.batches@[j].entries@, change())
        &&& forall|i: int, j: int| 0 <= i < j < self.unslotted@.len() ==> self.unslotted@[i] != self.unslotted@[j]
        &&& forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s)
    }

    /// E4: a batch out of a slot with nothing left to commit is due to be pruned (but `skip`,
    /// just made and about to be proposed).
    pub open spec fn due(self, skip: int) -> bool {
        !self.prune_due ==> forall|j: int| 0 <= j < self.batches@.len() && (#[trigger] self.batches@[j]).slot is None
            && self.batches@[j].seq != skip ==>
            exists|i: int| 0 <= i < self.batches@[j].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[j].entries@[i]))
    }

    pub open spec fn inv_but(self, skip: int) -> bool {
        &&& shape(self.v(), self.me, self.inc)
        &&& e1(self.v(), self.me, self.inc)
        &&& self.counted()
        &&& self.due(skip)
        &&& forall|j: int| 0 <= j < self.batches@.len() ==> (#[trigger] self.batches@[j]).seq < self.next_batch
    }

    pub open spec fn inv(self) -> bool { self.inv_but(-1) }

    /// No own entry is in dedup: whatever committed was resolved (E2).
    pub open spec fn settled(self) -> bool {
        forall|k: int| 0 <= k < self.own@.len() ==> !self.dedup@.contains((self.me, self.inc, (#[trigger] self.own@[k]).id))
    }

    /// What most calls require: the invariant, everything committed resolved, not too many
    /// entries held, and room for another batch.
    pub open spec fn calm(self) -> bool {
        &&& self.inv()
        &&& self.settled()
        &&& held(self.v()) <= held_max()
    }

    /// Calm, with `dedup` the dedup state of the last call.
    pub open spec fn ready(self, dedup: Dedup) -> bool {
        &&& self.calm()
        &&& forall|k: (u64, u64, u64)| dedup.has(k) <==> self.dedup@.contains(k)
    }

    /// E2 and E3 between two states: own records leave only as `left`; entries leave only
    /// into dedup or `out`.
    pub open spec fn kept(self, prev: Entries, left: Set<u64>, out: Seq<Item>) -> bool {
        &&& forall|id: u64| has_own(prev.v(), id) ==> has_own(self.v(), id) || left.contains(id)
        &&& forall|id: u64| left.contains(id) ==> !has_own(self.v(), id)
        &&& forall|k: (u64, u64, u64)| holds(prev.v(), k) ==>
            holds(self.v(), k) || self.dedup@.contains(k) || exists|i: int| 0 <= i < out.len() && key(#[trigger] out[i]) == k
    }

    pub open spec fn same_ids(self, prev: Entries) -> bool {
        &&& self.me == prev.me && self.inc == prev.inc && self.dedup == prev.dedup
        &&& forall|id: u64| has_own(self.v(), id) ==> has_own(prev.v(), id)
    }

    /// What every primitive keeps but the parts it changes.
    pub open spec fn frame(self, prev: Entries) -> bool {
        &&& self.me == prev.me && self.inc == prev.inc && self.dedup == prev.dedup
        &&& self.next_batch == prev.next_batch && self.prune_due == prev.prune_due
    }

    /// Whether there is something to propose: a batch waiting for a slot, or entries `cut`
    /// takes (own pending ones only when proposing them directly).
    pub fn proposable(&self, direct: bool) -> (r: bool)
        requires self.inv(),
        ensures r == (exists|s: u64| waits(self.batches@, s)) || self.relayed@.len() > 0 || (direct && self.pending@.len() > 0),
    {
        if self.unslotted.len() > 0 {
            assert(self.unslotted@.contains(self.unslotted@[0]));
            assert(waits(self.batches@, self.unslotted@[0]));
        }
        proof {
            if exists|s: u64| waits(self.batches@, s) {
                let s = choose|s: u64| waits(self.batches@, s);
                assert(self.unslotted@.contains(s));
            }
        }
        self.unslotted.len() > 0 || self.relayed.len() > 0 || (direct && self.pending.len() > 0)
    }
}

// ---- framing ----

/// `holds` reads the entries only.
pub proof fn lemma_holds_frame(v: V, w: V, k: (u64, u64, u64))
    requires
        w.pending == v.pending, w.relayed == v.relayed, w.fwd == v.fwd, w.batches.len() == v.batches.len(),
        forall|b: int| 0 <= b < v.batches.len() ==> (#[trigger] w.batches[b]).entries == v.batches[b].entries,
    ensures holds(v, k) == holds(w, k),
{
    if exists|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        assert(w.batches[b].entries[i] == v.batches[b].entries[i]);
    }
    if exists|b: int, i: int| 0 <= b < w.batches.len() && 0 <= i < w.batches[b].entries.len() && key(#[trigger] w.batches[b].entries[i]) == k {
        let (b, i) = choose|b: int, i: int| 0 <= b < w.batches.len() && 0 <= i < w.batches[b].entries.len() && key(#[trigger] w.batches[b].entries[i]) == k;
        assert(w.batches[b].entries[i] == v.batches[b].entries[i]);
    }
}

/// Batch seqs are keys.
pub proof fn lemma_one_seq(v: V, me: u64, inc: u64, i: int, j: int)
    requires shape(v, me, inc), 0 <= i < v.batches.len(), 0 <= j < v.batches.len(), v.batches[i].seq == v.batches[j].seq,
    ensures i == j,
{
    if i < j { assert(v.batches[i].seq != v.batches[j].seq); }
    if j < i { assert(v.batches[j].seq != v.batches[i].seq); }
}

/// With the same queues and batches, `holds` from either of them.
pub proof fn lemma_holds_same_queues(v: V, w: V, k: (u64, u64, u64))
    requires holds(v, k), !fwd_has(v.fwd, k), w.pending == v.pending, w.relayed == v.relayed, w.batches == v.batches,
    ensures holds(w, k),
{
    if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
        assert(key(w.pending[i]) == k);
    } else if exists|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k {
        let i = choose|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k;
        assert(key(w.relayed[i]) == k);
    } else if exists|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        assert(key(w.batches[b].entries[i]) == k);
    } else {
        let i = choose|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k;
        assert(fwd_has(v.fwd, k));
    }
}

/// What a removed forwarded entry held is held elsewhere or was it.
pub proof fn lemma_holds_fwd_remove(v: V, j: int, k: (u64, u64, u64))
    requires 0 <= j < v.fwd.len(), holds(v, k),
    ensures holds(V { fwd: v.fwd.remove(j), ..v }, k) || key(v.fwd[j].0) == k,
{
    let w = V { fwd: v.fwd.remove(j), ..v };
    if exists|a: int| 0 <= a < v.fwd.len() && key((#[trigger] v.fwd[a]).0) == k {
        let a = choose|a: int| 0 <= a < v.fwd.len() && key((#[trigger] v.fwd[a]).0) == k;
        if a != j {
            let a2 = if a < j { a } else { a - 1 };
            assert(w.fwd[a2] == v.fwd[a]);
        }
    } else {
        lemma_holds_same_queues(v, w, k);
    }
}

/// What a dropped batch held is held elsewhere or was in it.
pub proof fn lemma_holds_drop(v: V, j: int, k: (u64, u64, u64))
    requires 0 <= j < v.batches.len(), holds(v, k),
    ensures
        holds(V { batches: v.batches.remove(j), ..v }, k)
            || exists|i: int| 0 <= i < v.batches[j].entries.len() && key(#[trigger] v.batches[j].entries[i]) == k,
{
    let w = V { batches: v.batches.remove(j), ..v };
    if exists|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        if b != j {
            let b2 = if b < j { b } else { b - 1 };
            assert(w.batches[b2] == v.batches[b]);
            assert(key(w.batches[b2].entries[i]) == k);
        } else {
            assert(key(v.batches[j].entries[i]) == k);
        }
    } else if exists|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k {
        let i = choose|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k;
        assert(key(w.fwd[i].0) == k);
    } else if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
        assert(key(w.pending[i]) == k);
    } else {
        let i = choose|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k;
        assert(key(w.relayed[i]) == k);
    }
}

/// Removing own record `k` keeps E1 when every other record is fine.
pub proof fn lemma_own_remove(m: V, me: u64, inc: u64, k: int)
    requires
        unique_own(m.own), 0 <= k < m.own.len(),
        forall|j: int| 0 <= j < m.own.len() && j != k ==> fine(m, me, inc, #[trigger] m.own[j]),
    ensures ({
        let w = V { own: m.own.remove(k), ..m };
        &&& e1(w, me, inc) && unique_own(w.own)
        &&& forall|i: u64| has_own(w, i) <==> has_own(m, i) && i != m.own[k].id
    }),
{
    let w = V { own: m.own.remove(k), ..m };
    assert forall|j: int| 0 <= j < w.own.len() implies fine(w, me, inc, #[trigger] w.own[j]) by {
        let j2 = if j < k { j } else { j + 1 };
        assert(w.own[j] == m.own[j2]);
        assert(fine(m, me, inc, m.own[j2]));
    }
    assert forall|a: int, b: int| 0 <= a < b < w.own.len() implies w.own[a].id != w.own[b].id by {
        let a2 = if a < k { a } else { a + 1 };
        let b2 = if b < k { b } else { b + 1 };
        assert(w.own[a] == m.own[a2] && w.own[b] == m.own[b2]);
    }
    assert forall|i: u64| has_own(w, i) <==> has_own(m, i) && i != m.own[k].id by {
        if has_own(w, i) {
            let j = choose|j: int| 0 <= j < w.own.len() && w.own[j].id == i;
            let j2 = if j < k { j } else { j + 1 };
            assert(w.own[j] == m.own[j2]);
            if j2 < k { assert(m.own[j2].id != m.own[k].id); }
            if j2 > k { assert(m.own[k].id != m.own[j2].id); }
        }
        if has_own(m, i) && i != m.own[k].id {
            let j = choose|j: int| 0 <= j < m.own.len() && m.own[j].id == i;
            let j2 = if j < k { j } else { j - 1 };
            assert(w.own[j2] == m.own[j]);
        }
    }
}

/// Dropping the forwarded entry of `id` leaves every other own entry fine.
pub proof fn lemma_others_fine(v: V, m: V, me: u64, inc: u64, id: u64)
    requires
        shape(v, me, inc), e1(v, me, inc), shape(m, me, inc),
        m.pending == v.pending, m.relayed == v.relayed, m.batches == v.batches, m.own == v.own,
        fmap(m.fwd) == fmap(v.fwd).remove(id),
    ensures
        e1(v, me, inc) && shape(m, me, inc),
        forall|i: u64| i != id ==> #[trigger] census(m, me, inc, i) == census(v, me, inc, i),
        forall|j: int| 0 <= j < m.own.len() && (#[trigger] m.own[j]).id != id ==> fine(m, me, inc, m.own[j]),
        forall|j: int| 0 <= j < m.own.len() && (forall|a: int| 0 <= a < m.own.len() ==> (#[trigger] m.own[a]).id != id) ==> fine(m, me, inc, #[trigger] m.own[j]),
{
    assert forall|i: u64| i != id implies #[trigger] census(m, me, inc, i) == census(v, me, inc, i) by {}
    assert forall|j: int| 0 <= j < m.own.len() && (#[trigger] m.own[j]).id != id implies fine(m, me, inc, m.own[j]) by {
        assert(fine(v, me, inc, v.own[j]));
        assert(census(m, me, inc, m.own[j].id) == census(v, me, inc, m.own[j].id));
    }
}

/// Adding a batch with no copy of any own entry, under a fresh seq, keeps E1.
pub proof fn lemma_e1_add_batch(v: V, me: u64, inc: u64, nb: BatchV)
    requires
        e1(v, me, inc), shape(v, me, inc),
        forall|j: int| 0 <= j < v.batches.len() ==> (#[trigger] v.batches[j]).seq != nb.seq,
        forall|k: int| 0 <= k < v.own.len() ==> count(nb.entries, this(me, inc, (#[trigger] v.own[k]).id)) == 0,
    ensures e1(V { batches: v.batches.push(nb), ..v }, me, inc), shape(V { batches: v.batches.push(nb), ..v }, me, inc),
{
    let w = V { batches: v.batches.push(nb), ..v };
    lemma_bmap_push(v.batches, nb);
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        let o = v.own[k];
        assert(fine(v, me, inc, o));
        let (c, d) = (census(v, me, inc, o.id), census(w, me, inc, o.id));
        assert(d.b == c.b.insert(nb.seq, bentry(nb, this(me, inc, o.id))));
        if let At::Batched { seq } = o.at {
            lemma_bmap(v.batches, this(me, inc, o.id));
            if seq == nb.seq {
                let j = choose|j: int| 0 <= j < v.batches.len() && v.batches[j].seq == seq;
            }
        }
        if !o.copied {
            assert forall|s: u64| #[trigger] d.b.contains_key(s) && (!d.b[s].0 || o.at == (At::Batched { seq: s })) implies
                d.b[s].1 == if o.at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                if s != nb.seq { assert(c.b.contains_key(s)); }
            }
        }
    }
}

/// Removing an empty batch keeps E1.
pub proof fn lemma_drop_empty_batch(w: V, v: V, me: u64, inc: u64)
    requires
        e1(v, me, inc), shape(v, me, inc), v.batches.len() == w.batches.len() + 1,
        v == (V { batches: w.batches.push(v.batches.last()), ..w }), v.batches.last().entries.len() == 0,
    ensures e1(w, me, inc), shape(w, me, inc),
{
    let nb = v.batches.last();
    assert(unique_seqs(w.batches)) by {
        assert forall|i: int, j: int| 0 <= i < j < w.batches.len() implies w.batches[i].seq != w.batches[j].seq by {
            assert(v.batches[i] == w.batches[i] && v.batches[j] == w.batches[j]);
        }
    }
    assert forall|j: int| 0 <= j < w.batches.len() implies (#[trigger] w.batches[j]).seq != nb.seq by {
        assert(v.batches[j] == w.batches[j]);
        assert(v.batches[j].seq != v.batches[w.batches.len() as int].seq);
    }
    lemma_bmap_push(w.batches, nb);
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        let o = w.own[k];
        assert(fine(v, me, inc, o));
        let (c, d) = (census(w, me, inc, o.id), census(v, me, inc, o.id));
        assert(nb.entries =~= Seq::<Item>::empty());
        assert(count(nb.entries, this(me, inc, o.id)) == 0);
        assert(d.b == c.b.insert(nb.seq, bentry(nb, this(me, inc, o.id))));
        if !o.copied {
            assert forall|s: u64| #[trigger] c.b.contains_key(s) && (!c.b[s].0 || o.at == (At::Batched { seq: s })) implies
                c.b[s].1 == if o.at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                lemma_bmap(w.batches, this(me, inc, o.id));
                let j = choose|j: int| 0 <= j < w.batches.len() && w.batches[j].seq == s;
                assert(s != nb.seq);
                assert(d.b.contains_key(s));
            }
        }
    }
}

/// `holds` survives more batches and longer ones.
pub proof fn lemma_holds_grow(v: V, w: V, k: (u64, u64, u64))
    requires
        holds(v, k), w.pending == v.pending, w.relayed == v.relayed, w.fwd == v.fwd, w.batches.len() >= v.batches.len(),
        forall|b: int| 0 <= b < v.batches.len() ==> (#[trigger] w.batches[b]).entries.len() >= v.batches[b].entries.len()
            && forall|i: int| 0 <= i < v.batches[b].entries.len() ==> w.batches[b].entries[i] == v.batches[b].entries[i],
    ensures holds(w, k),
{
    if exists|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        assert(w.batches[b].entries[i] == v.batches[b].entries[i]);
    } else if exists|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k {
        let i = choose|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k;
        assert(key(w.fwd[i].0) == k);
    } else if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
        assert(key(w.pending[i]) == k);
    } else {
        let i = choose|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k;
        assert(key(w.relayed[i]) == k);
    }
}

/// `holds` survives a batch added after the others.
pub proof fn lemma_holds_prefix(v: V, w: V, k: (u64, u64, u64))
    requires
        holds(v, k), w.pending == v.pending, w.relayed == v.relayed, w.fwd == v.fwd, w.batches.len() >= v.batches.len(),
        forall|b: int| 0 <= b < v.batches.len() ==> #[trigger] w.batches[b] == v.batches[b],
    ensures holds(w, k),
{
    lemma_holds_grow(v, w, k);
}

/// What a queue's head held is held by the rest, or was the head.
pub proof fn lemma_holds_pop(v0: V, v1: V, relayed: bool, k: (u64, u64, u64))
    requires
        holds(v0, k), v1.batches == v0.batches, v1.fwd == v0.fwd,
        relayed ==> v0.relayed.len() > 0 && v1.relayed == v0.relayed.subrange(1, v0.relayed.len() as int) && v1.pending == v0.pending,
        !relayed ==> v0.pending.len() > 0 && v1.pending == v0.pending.subrange(1, v0.pending.len() as int) && v1.relayed == v0.relayed,
    ensures holds(v1, k) || k == key(if relayed { v0.relayed[0] } else { v0.pending[0] }),
{
    if exists|i: int| 0 <= i < v0.pending.len() && key(#[trigger] v0.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v0.pending.len() && key(#[trigger] v0.pending[i]) == k;
        if relayed { assert(key(v1.pending[i]) == k); } else if i > 0 { assert(v1.pending[i - 1] == v0.pending[i]); }
    } else if exists|i: int| 0 <= i < v0.relayed.len() && key(#[trigger] v0.relayed[i]) == k {
        let i = choose|i: int| 0 <= i < v0.relayed.len() && key(#[trigger] v0.relayed[i]) == k;
        if !relayed { assert(key(v1.relayed[i]) == k); } else if i > 0 { assert(v1.relayed[i - 1] == v0.relayed[i]); }
    } else if exists|i: int| 0 <= i < v0.fwd.len() && key((#[trigger] v0.fwd[i]).0) == k {
        let i = choose|i: int| 0 <= i < v0.fwd.len() && key((#[trigger] v0.fwd[i]).0) == k;
        assert(key(v1.fwd[i].0) == k);
    } else {
        let (b, i) = choose|b: int, i: int| 0 <= b < v0.batches.len() && 0 <= i < v0.batches[b].entries.len() && key(#[trigger] v0.batches[b].entries[i]) == k;
        assert(key(v1.batches[b].entries[i]) == k);
    }
}

/// E1 reads of own records only their ids, places and `copied`.
pub proof fn lemma_e1_own_fields(v: V, w: V, me: u64, inc: u64)
    requires
        e1(v, me, inc), unique_own(v.own),
        w.pending == v.pending, w.relayed == v.relayed, w.fwd == v.fwd, w.batches == v.batches, w.own.len() == v.own.len(),
        forall|k: int| 0 <= k < v.own.len() ==> (#[trigger] w.own[k]).id == v.own[k].id && w.own[k].at == v.own[k].at
            && w.own[k].copied == v.own[k].copied && w.own[k].change == v.own[k].change,
    ensures e1(w, me, inc), unique_own(w.own), ocount(w.own) == ocount(v.own),
{
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        assert(fine(v, me, inc, v.own[k]));
        assert(census(w, me, inc, w.own[k].id) == census(v, me, inc, v.own[k].id));
    }
    assert forall|a: int, b: int| 0 <= a < b < w.own.len() implies w.own[a].id != w.own[b].id by {
        assert(v.own[a].id != v.own[b].id);
    }
    lemma_ocount_same(v.own, w.own);
}

/// The census does not depend on where a batch sits among the others.
pub proof fn lemma_e1_same_census(v: V, w: V, me: u64, inc: u64)
    requires e1(v, me, inc), w.own == v.own, forall|i: u64| #[trigger] census(w, me, inc, i) == census(v, me, inc, i),
    ensures e1(w, me, inc),
{
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        assert(fine(v, me, inc, v.own[k]));
    }
}

/// A batch taken out to be handed back: counted, in a slot, after the others.
pub open spec fn vback(v: V, seq: u64, rest: Seq<Item>) -> V {
    V { batches: v.batches.push(BatchV { seq, entries: rest, slot: Some(0) }), ..v }
}

/// Taking batch `j` out to hand it back keeps E1, counting its entries as in a slot.
pub proof fn lemma_e1_back(v: V, me: u64, inc: u64, j: int)
    requires e1(v, me, inc), shape(v, me, inc), 0 <= j < v.batches.len(),
    ensures ({
        let w = vback(V { batches: v.batches.remove(j), ..v }, v.batches[j].seq, v.batches[j].entries);
        e1(w, me, inc) && shape(w, me, inc)
    }),
{
    let b = v.batches[j];
    let nb = BatchV { seq: b.seq, entries: b.entries, slot: Some(0) };
    let u = V { batches: v.batches.update(j, nb), ..v };
    let r = V { batches: v.batches.remove(j), ..v };
    let w = vback(r, b.seq, b.entries);
    lemma_e1_slotted(v, me, inc, j, 0);
    lemma_bmap_update(v.batches, j, nb);
    lemma_bmap_remove(v.batches, j);
    assert forall|i: int| 0 <= i < r.batches.len() implies (#[trigger] r.batches[i]).seq != b.seq by {
        let i2 = if i < j { i } else { i + 1 };
        assert(r.batches[i] == v.batches[i2]);
        assert(v.batches[i2].seq != v.batches[j].seq) by { if i2 < j {} else {} }
    }
    lemma_bmap_push(r.batches, nb);
    assert forall|i: u64| #[trigger] census(w, me, inc, i) == census(u, me, inc, i) by {
        let x = this(me, inc, i);
        assert(bmap(w.batches, x) =~= bmap(u.batches, x));
    }
    lemma_e1_same_census(u, w, me, inc);
}

/// `holds` survives queues growing at the back.
pub proof fn lemma_holds_extend(v: V, w: V, k: (u64, u64, u64))
    requires
        holds(v, k), w.fwd == v.fwd, w.batches == v.batches,
        w.pending.len() >= v.pending.len(), w.pending.subrange(0, v.pending.len() as int) == v.pending,
        w.relayed.len() >= v.relayed.len(), w.relayed.subrange(0, v.relayed.len() as int) == v.relayed,
    ensures holds(w, k),
{
    if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
        assert(w.pending.subrange(0, v.pending.len() as int)[i] == w.pending[i]);
    } else if exists|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k {
        let i = choose|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k;
        assert(w.relayed.subrange(0, v.relayed.len() as int)[i] == w.relayed[i]);
    } else if exists|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k {
        let i = choose|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k;
        assert(key(w.fwd[i].0) == k);
    } else {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        assert(key(w.batches[b].entries[i]) == k);
    }
}

/// A prefix of the batches holds no more entries than all of them.
pub proof fn lemma_bsum_prefix(bs: Seq<BatchV>, j: int)
    requires 0 <= j <= bs.len(),
    ensures bsum(bs.subrange(0, j), every()) <= bsum(bs, every()),
{
    lemma_bsum_add(bs.subrange(0, j), bs.subrange(j, bs.len() as int), every());
    assert(bs.subrange(0, j) + bs.subrange(j, bs.len() as int) =~= bs);
}

/// `holds` survives an entry put at the front of `pending`.
pub proof fn lemma_holds_front(v: V, w: V, x: Item, k: (u64, u64, u64))
    requires holds(v, k), w.pending == v.pending.insert(0, x), w.relayed == v.relayed, w.fwd == v.fwd, w.batches == v.batches,
    ensures holds(w, k),
{
    if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
        let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
        assert(w.pending[i + 1] == v.pending[i]);
    } else if exists|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k {
        let i = choose|i: int| 0 <= i < v.relayed.len() && key(#[trigger] v.relayed[i]) == k;
        assert(key(w.relayed[i]) == k);
    } else if exists|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k {
        let i = choose|i: int| 0 <= i < v.fwd.len() && key((#[trigger] v.fwd[i]).0) == k;
        assert(key(w.fwd[i].0) == k);
    } else {
        let (b, i) = choose|b: int, i: int| 0 <= b < v.batches.len() && 0 <= i < v.batches[b].entries.len() && key(#[trigger] v.batches[b].entries[i]) == k;
        assert(key(w.batches[b].entries[i]) == k);
    }
}

pub proof fn lemma_views_push(bs: Seq<Batch>, b: Batch)
    ensures views(bs.push(b)) == views(bs).push(bv(b)),
{
    assert(views(bs.push(b)) =~= views(bs).push(bv(b)));
}

pub proof fn lemma_views_remove(bs: Seq<Batch>, j: int)
    requires 0 <= j < bs.len(),
    ensures views(bs.remove(j)) == views(bs).remove(j),
{
    assert(views(bs.remove(j)) =~= views(bs).remove(j));
}

/// An entry not held anywhere has no copy anywhere.
pub proof fn lemma_absent(v: V, me: u64, inc: u64, id: u64)
    requires shape(v, me, inc), !holds(v, (me, inc, id)),
    ensures ({
        let c = census(v, me, inc, id);
        &&& c.p == 0 && c.r == 0 && c.f is None
        &&& forall|s: u64| #[trigger] c.b.contains_key(s) ==> c.b[s].1 == 0
    }),
{
    let x = this(me, inc, id);
    lemma_count_pos(v.pending, x);
    lemma_count_pos(v.relayed, x);
    if count(v.pending, x) > 0 {
        let i = choose|i: int| 0 <= i < v.pending.len() && x(#[trigger] v.pending[i]);
        assert(key(v.pending[i]) == (me, inc, id));
    }
    if count(v.relayed, x) > 0 {
        let i = choose|i: int| 0 <= i < v.relayed.len() && x(#[trigger] v.relayed[i]);
        assert(key(v.relayed[i]) == (me, inc, id));
    }
    lemma_fmap(v.fwd);
    if fmap(v.fwd).contains_key(id) {
        let j = choose|j: int| 0 <= j < v.fwd.len() && v.fwd[j].0.id == id;
        assert(key(v.fwd[j].0) == (me, inc, id));
    }
    lemma_bmap(v.batches, x);
    assert forall|s: u64| #[trigger] bmap(v.batches, x).contains_key(s) implies bmap(v.batches, x)[s].1 == 0 by {
        let j = choose|j: int| 0 <= j < v.batches.len() && v.batches[j].seq == s;
        assert(bmap(v.batches, x).contains_key(v.batches[j].seq));
        lemma_count_pos(v.batches[j].entries, x);
        if count(v.batches[j].entries, x) > 0 {
            let i = choose|i: int| 0 <= i < v.batches[j].entries.len() && x(#[trigger] v.batches[j].entries[i]);
            assert(key(v.batches[j].entries[i]) == (me, inc, id));
        }
    }
}

/// Batch `seq` is among `bs` and was handed on.
pub open spec fn handed_on(bs: Seq<Batch>, seq: u64) -> bool {
    exists|j: int| 0 <= j < bs.len() && (#[trigger] bs[j]).seq == seq && bs[j].handed
}

/// An entry with key `k` is in batch `seq`.
pub open spec fn in_batch(v: V, seq: u64, k: (u64, u64, u64)) -> bool {
    exists|j: int, i: int| 0 <= j < v.batches.len() && v.batches[j].seq == seq && 0 <= i < v.batches[j].entries.len()
        && key(#[trigger] v.batches[j].entries[i]) == k
}

/// `At::same`: the same place, whenever a forwarded entry was handed on.
pub open spec fn same(a: At, b: At) -> bool {
    match (a, b) {
        (At::Forwarded { .. }, At::Forwarded { .. }) => true,
        _ => a == b,
    }
}

fn same_exec(a: At, b: At) -> (r: bool)
    ensures r == same(a, b),
{
    match (a, b) {
        (At::Forwarded { .. }, At::Forwarded { .. }) => true,
        (At::Pending, At::Pending) => true,
        (At::Relayed, At::Relayed) => true,
        (At::Batched { seq: s }, At::Batched { seq: t }) => s == t,
        _ => false,
    }
}

/// A copied entry stays fine as copies of it are added in the queues.
pub proof fn lemma_fine_more(v: V, w: V, me: u64, inc: u64, o: Own)
    requires
        fine(v, me, inc, o), o.copied,
        census(w, me, inc, o.id).p >= census(v, me, inc, o.id).p,
        census(w, me, inc, o.id).r >= census(v, me, inc, o.id).r,
        census(w, me, inc, o.id).f == census(v, me, inc, o.id).f,
        census(w, me, inc, o.id).b == census(v, me, inc, o.id).b,
    ensures fine(w, me, inc, o),
{
}

/// No own entry is in `d`.
pub open spec fn unresolved(own: Seq<Own>, d: Set<(u64, u64, u64)>, me: u64, inc: u64) -> bool {
    forall|k: int| 0 <= k < own.len() ==> !d.contains((me, inc, (#[trigger] own[k]).id))
}

/// Dropping a batch whose entries all committed keeps E1 while no own entry committed.
pub proof fn lemma_e1_drop(v: V, me: u64, inc: u64, j: int, d: Set<(u64, u64, u64)>)
    requires
        shape(v, me, inc), e1(v, me, inc), 0 <= j < v.batches.len(), unresolved(v.own, d, me, inc),
        forall|i: int| 0 <= i < v.batches[j].entries.len() ==> d.contains(key(#[trigger] v.batches[j].entries[i])),
    ensures
        e1(V { batches: v.batches.remove(j), ..v }, me, inc), shape(V { batches: v.batches.remove(j), ..v }, me, inc),
{
    let w = V { batches: v.batches.remove(j), ..v };
    lemma_bmap_remove(v.batches, j);
    assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
        let o = w.own[k];
        let x = this(me, inc, o.id);
        assert(fine(v, me, inc, o));
        assert(!d.contains((me, inc, o.id)));
        lemma_count_pos(v.batches[j].entries, x);
        if count(v.batches[j].entries, x) > 0 {
            let i = choose|i: int| 0 <= i < v.batches[j].entries.len() && x(#[trigger] v.batches[j].entries[i]);
            assert(key(v.batches[j].entries[i]) == (me, inc, o.id));
        }
        lemma_bmap(v.batches, x);
        assert(bmap(v.batches, x).contains_key(v.batches[j].seq));
        let (c, cw) = (census(v, me, inc, o.id), census(w, me, inc, o.id));
        assert(cw.b == c.b.remove(v.batches[j].seq));
        if let At::Batched { seq } = o.at {
            assert(seq != v.batches[j].seq);
        }
        if !o.copied {
            assert forall|s: u64| #[trigger] cw.b.contains_key(s) && (!cw.b[s].0 || o.at == (At::Batched { seq: s })) implies
                cw.b[s].1 == if o.at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                assert(c.b.contains_key(s));
            }
        }
    }
}

impl Entries {
    fn find_own(&self, id: u64) -> (r: Option<usize>)
        ensures
            r matches Some(k) ==> k < self.own@.len() && self.own@[k as int].id == id,
            r is None ==> forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id != id,
    {
        let mut k = 0;
        while k < self.own.len()
            invariant k <= self.own@.len(), forall|i: int| 0 <= i < k ==> (#[trigger] self.own@[i]).id != id,
            decreases self.own.len() - k,
        {
            if self.own[k].id == id {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    fn find_fwd(&self, id: u64) -> (r: Option<usize>)
        ensures
            r matches Some(k) ==> k < self.forwarded@.len() && self.forwarded@[k as int].0.id == id,
            r is None ==> forall|k: int| 0 <= k < self.forwarded@.len() ==> (#[trigger] self.forwarded@[k]).0.id != id,
    {
        let mut k = 0;
        while k < self.forwarded.len()
            invariant k <= self.forwarded@.len(), forall|i: int| 0 <= i < k ==> (#[trigger] self.forwarded@[i]).0.id != id,
            decreases self.forwarded.len() - k,
        {
            if self.forwarded[k].0.id == id {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    fn find_batch(&self, seq: u64) -> (r: Option<usize>)
        ensures
            r matches Some(j) ==> j < self.batches@.len() && self.batches@[j as int].seq == seq,
            r is None ==> forall|j: int| 0 <= j < self.batches@.len() ==> (#[trigger] self.batches@[j]).seq != seq,
    {
        let mut j = 0;
        while j < self.batches.len()
            invariant j <= self.batches@.len(), forall|i: int| 0 <= i < j ==> (#[trigger] self.batches@[i]).seq != seq,
            decreases self.batches.len() - j,
        {
            if self.batches[j].seq == seq {
                return Some(j);
            }
            j += 1;
        }
        None
    }

    fn find_unslotted(&self, seq: u64) -> (r: Option<usize>)
        ensures
            r matches Some(i) ==> i < self.unslotted@.len() && self.unslotted@[i as int] == seq,
            r is None ==> !self.unslotted@.contains(seq),
    {
        let mut i = 0;
        while i < self.unslotted.len()
            invariant i <= self.unslotted@.len(), forall|j: int| 0 <= j < i ==> self.unslotted@[j] != seq,
            decreases self.unslotted.len() - i,
        {
            if self.unslotted[i] == seq {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// `unslotted.remove(&seq)`.
    fn unslotted_remove(&mut self, seq: u64)
        requires forall|i: int, j: int| 0 <= i < j < old(self).unslotted@.len() ==> old(self).unslotted@[i] != old(self).unslotted@[j],
        ensures
            forall|i: int, j: int| 0 <= i < j < final(self).unslotted@.len() ==> final(self).unslotted@[i] != final(self).unslotted@[j],
            forall|s: u64| final(self).unslotted@.contains(s) <==> old(self).unslotted@.contains(s) && s != seq,
            final(self).batches == old(self).batches, final(self).pending == old(self).pending,
            final(self).relayed == old(self).relayed, final(self).forwarded == old(self).forwarded,
            final(self).own == old(self).own, final(self).foreign == old(self).foreign,
            final(self).changes == old(self).changes, final(self).prune_due == old(self).prune_due,
            final(self).next_batch == old(self).next_batch, final(self).me == old(self).me,
            final(self).inc == old(self).inc, final(self).dedup == old(self).dedup,
    {
        if let Some(i) = self.find_unslotted(seq) {
            let ghost u = self.unslotted@;
            self.unslotted.remove(i);
            assert(self.unslotted@ =~= u.remove(i as int));
            assert forall|a: int, b: int| 0 <= a < b < self.unslotted@.len() implies self.unslotted@[a] != self.unslotted@[b] by {
                let a2 = if a < i { a } else { a + 1 };
                let b2 = if b < i { b } else { b + 1 };
                assert(self.unslotted@[a] == u[a2] && self.unslotted@[b] == u[b2]);
            }
            assert forall|s: u64| self.unslotted@.contains(s) <==> u.contains(s) && s != seq by {
                if u.contains(s) && s != seq {
                    let j = choose|j: int| 0 <= j < u.len() && u[j] == s;
                    if j < i { assert(self.unslotted@[j] == s); } else { assert(self.unslotted@[j - 1] == s); }
                }
                if self.unslotted@.contains(s) {
                    let j = choose|j: int| 0 <= j < self.unslotted@.len() && self.unslotted@[j] == s;
                    let j2 = if j < i { j } else { j + 1 };
                    assert(u[j2] == s);
                    assert(j2 != i);
                }
            }
        }
    }

    /// `unslotted.insert(seq)`.
    fn unslotted_insert(&mut self, seq: u64)
        requires forall|i: int, j: int| 0 <= i < j < old(self).unslotted@.len() ==> old(self).unslotted@[i] != old(self).unslotted@[j],
        ensures
            forall|i: int, j: int| 0 <= i < j < final(self).unslotted@.len() ==> final(self).unslotted@[i] != final(self).unslotted@[j],
            forall|s: u64| final(self).unslotted@.contains(s) <==> old(self).unslotted@.contains(s) || s == seq,
            final(self).batches == old(self).batches, final(self).pending == old(self).pending,
            final(self).relayed == old(self).relayed, final(self).forwarded == old(self).forwarded,
            final(self).own == old(self).own, final(self).foreign == old(self).foreign,
            final(self).changes == old(self).changes, final(self).prune_due == old(self).prune_due,
            final(self).next_batch == old(self).next_batch, final(self).me == old(self).me,
            final(self).inc == old(self).inc, final(self).dedup == old(self).dedup,
    {
        if self.find_unslotted(seq).is_some() {
            return;
        }
        let ghost u = self.unslotted@;
        self.unslotted.push(seq);
        assert forall|s: u64| self.unslotted@.contains(s) <==> u.contains(s) || s == seq by {
            if u.contains(s) {
                let j = choose|j: int| 0 <= j < u.len() && u[j] == s;
                assert(self.unslotted@[j] == s);
            }
            if s == seq { assert(self.unslotted@[u.len() as int] == s); }
            if self.unslotted@.contains(s) && s != seq {
                let j = choose|j: int| 0 <= j < self.unslotted@.len() && self.unslotted@[j] == s;
                assert(u[j] == s);
            }
        }
        assert forall|a: int, b: int| 0 <= a < b < self.unslotted@.len() implies self.unslotted@[a] != self.unslotted@[b] by {
            if b == u.len() { assert(u[a] == self.unslotted@[a]); assert(u.contains(u[a])); }
        }
    }

    /// Batch `j`'s slot becomes `slot`. DEV: out and back in for `get_mut`.
    fn set_slot(&mut self, j: usize, slot: Option<u64>)
        requires j < old(self).batches@.len(),
        ensures
            final(self).batches@.len() == old(self).batches@.len(),
            forall|i: int| 0 <= i < final(self).batches@.len() && i != j ==> final(self).batches@[i] == old(self).batches@[i],
            final(self).batches@[j as int].seq == old(self).batches@[j as int].seq,
            final(self).batches@[j as int].entries@ == old(self).batches@[j as int].entries@,
            final(self).batches@[j as int].changes == old(self).batches@[j as int].changes,
            final(self).batches@[j as int].slot == slot,
            final(self).batches@[j as int].handed == old(self).batches@[j as int].handed,
            views(final(self).batches@) == views(old(self).batches@).update(j as int, BatchV { seq: old(self).batches@[j as int].seq, entries: old(self).batches@[j as int].entries@, slot }),
            final(self).unslotted == old(self).unslotted, final(self).pending == old(self).pending,
            final(self).relayed == old(self).relayed, final(self).forwarded == old(self).forwarded,
            final(self).own == old(self).own, final(self).foreign == old(self).foreign,
            final(self).changes == old(self).changes, final(self).prune_due == old(self).prune_due,
            final(self).next_batch == old(self).next_batch, final(self).me == old(self).me,
            final(self).inc == old(self).inc, final(self).dedup == old(self).dedup,
    {
        let b = self.batches.remove(j);
        let Batch { seq, entries, slot: _, changes, handed } = b;
        self.batches.insert(j, Batch { seq, entries, slot, changes, handed });
        assert(views(self.batches@) =~= views(old(self).batches@).update(j as int, BatchV { seq: old(self).batches@[j as int].seq, entries: old(self).batches@[j as int].entries@, slot }));
    }

    /// Batch `j` is marked handed on. DEV: out and back in for `get_mut`.
    fn set_handed(&mut self, j: usize)
        requires j < old(self).batches@.len(),
        ensures
            final(self).batches@.len() == old(self).batches@.len(),
            forall|i: int| 0 <= i < final(self).batches@.len() && i != j ==> final(self).batches@[i] == old(self).batches@[i],
            final(self).batches@[j as int].seq == old(self).batches@[j as int].seq,
            final(self).batches@[j as int].entries@ == old(self).batches@[j as int].entries@,
            final(self).batches@[j as int].changes == old(self).batches@[j as int].changes,
            final(self).batches@[j as int].slot == old(self).batches@[j as int].slot,
            final(self).batches@[j as int].handed,
            views(final(self).batches@) == views(old(self).batches@),
            final(self).unslotted == old(self).unslotted, final(self).pending == old(self).pending,
            final(self).relayed == old(self).relayed, final(self).forwarded == old(self).forwarded,
            final(self).own == old(self).own, final(self).foreign == old(self).foreign,
            final(self).changes == old(self).changes, final(self).prune_due == old(self).prune_due,
            final(self).next_batch == old(self).next_batch, final(self).me == old(self).me,
            final(self).inc == old(self).inc, final(self).dedup == old(self).dedup,
            old(self).inv() ==> final(self).inv(),
    {
        let b = self.batches.remove(j);
        let Batch { seq, entries, slot, changes, handed: _ } = b;
        self.batches.insert(j, Batch { seq, entries, slot, changes, handed: true });
        proof {
            let (o, n) = (old(self).batches@, self.batches@);
            assert(views(n) =~= views(o)) by {
                assert forall|i: int| 0 <= i < n.len() implies #[trigger] views(n)[i] == views(o)[i] by {
                    if i != j { assert(n[i] == o[i]); }
                }
            }
            assert forall|s: u64| waits(n, s) <==> waits(o, s) by {
                if waits(n, s) {
                    let i = choose|i: int| 0 <= i < n.len() && (#[trigger] n[i]).seq == s && n[i].slot is None && n[i].entries@.len() > 0;
                    if i != j { assert(n[i] == o[i]); }
                    assert(o[i].seq == s);
                }
                if waits(o, s) {
                    let i = choose|i: int| 0 <= i < o.len() && (#[trigger] o[i]).seq == s && o[i].slot is None && o[i].entries@.len() > 0;
                    if i != j { assert(n[i] == o[i]); }
                    assert(n[i].seq == s);
                }
            }
            assert forall|i: int| 0 <= i < n.len() implies (#[trigger] n[i]).changes == o[i].changes && n[i].seq == o[i].seq
                && n[i].slot == o[i].slot && n[i].entries@ == o[i].entries@ by {
                if i != j { assert(n[i] == o[i]); }
            }
        }
    }

    pub proof fn lemma_bounded(self)
        requires self.counted(),
        ensures self.changes <= held(self.v()), self.foreign <= held(self.v()),
    {
        let v = self.v();
        lemma_count_le(v.pending, change());
        lemma_count_le(v.relayed, change());
        lemma_count_le(v.relayed, foreign(self.me));
        lemma_bsum_le(v.batches, change());
        lemma_bsum_le(v.batches, foreign(self.me));
        lemma_ocount_le(v.own);
        assert(every() == |x: Item| true);
    }

    fn push_pending(&mut self, x: Item)
        requires old(self).counted(), old(self).changes < u64::MAX,
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).pending@ == old(self).pending@.push(x),
            final(self).relayed == old(self).relayed, final(self).forwarded == old(self).forwarded,
            final(self).batches == old(self).batches, final(self).own == old(self).own,
            final(self).unslotted == old(self).unslotted, final(self).foreign == old(self).foreign,
    {
        if x.change { self.changes = self.changes + 1; }
        self.pending.push(x);
        proof { lemma_count_push(old(self).pending@, x); }
    }

    fn push_relayed(&mut self, x: Item)
        requires old(self).counted(), old(self).changes < u64::MAX, old(self).foreign < u64::MAX,
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).relayed@ == old(self).relayed@.push(x),
            final(self).pending == old(self).pending, final(self).forwarded == old(self).forwarded,
            final(self).batches == old(self).batches, final(self).own == old(self).own,
            final(self).unslotted == old(self).unslotted,
    {
        if x.change { self.changes = self.changes + 1; }
        if x.origin != self.me { self.foreign = self.foreign + 1; }
        self.relayed.push(x);
        proof { lemma_count_push(old(self).relayed@, x); }
    }

    /// DEV: `remove(0)` for `pop_front`.
    fn pop(&mut self, relayed: bool) -> (r: Option<Item>)
        requires old(self).counted(),
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).forwarded == old(self).forwarded, final(self).batches == old(self).batches,
            final(self).own == old(self).own, final(self).unslotted == old(self).unslotted,
            relayed ==> final(self).pending == old(self).pending,
            !relayed ==> final(self).relayed == old(self).relayed,
            r is None ==> (if relayed { old(self).relayed@.len() == 0 } else { old(self).pending@.len() == 0 }) && final(self).relayed == old(self).relayed && final(self).pending == old(self).pending,
            r matches Some(x) ==> if relayed {
                old(self).relayed@.len() > 0 && x == old(self).relayed@[0] && final(self).relayed@ == old(self).relayed@.subrange(1, old(self).relayed@.len() as int)
            } else {
                old(self).pending@.len() > 0 && x == old(self).pending@[0] && final(self).pending@ == old(self).pending@.subrange(1, old(self).pending@.len() as int)
            },
    {
        if relayed {
            if self.relayed.len() == 0 { return None; }
            proof { lemma_count_pop(self.relayed@); }
            let x = self.relayed.remove(0);
            assert(self.relayed@ =~= old(self).relayed@.subrange(1, old(self).relayed@.len() as int));
            if x.change { self.changes = self.changes - 1; }
            if x.origin != self.me { self.foreign = self.foreign - 1; }
            Some(x)
        } else {
            if self.pending.len() == 0 { return None; }
            proof { lemma_count_pop(self.pending@); }
            let x = self.pending.remove(0);
            assert(self.pending@ =~= old(self).pending@.subrange(1, old(self).pending@.len() as int));
            if x.change { self.changes = self.changes - 1; }
            Some(x)
        }
    }

    /// Entries of other members among `ids`. DEV: an index loop for `filter().count()`.
    fn foreign_in(&self, ids: &Vec<Item>) -> (n: u64)
        ensures n == count(ids@, foreign(self.me)),
    {
        let mut n: u64 = 0;
        let mut i = 0;
        while i < ids.len()
            invariant i <= ids.len(), n == count(ids@.subrange(0, i as int), foreign(self.me)), n <= i,
            decreases ids.len() - i,
        {
            proof {
                lemma_count_push(ids@.subrange(0, i as int), ids@[i as int]);
                assert(ids@.subrange(0, i + 1) =~= ids@.subrange(0, i as int).push(ids@[i as int]));
            }
            if ids[i].origin != self.me { n = n + 1; }
            i += 1;
        }
        assert(ids@.subrange(0, ids.len() as int) =~= ids@);
        n
    }

    /// Membership changes among `ids`. DEV: an index loop for `filter().count()`.
    fn changes_in(ids: &Vec<Item>) -> (n: u64)
        ensures n == count(ids@, change()),
    {
        let mut n: u64 = 0;
        let mut i = 0;
        while i < ids.len()
            invariant i <= ids.len(), n == count(ids@.subrange(0, i as int), change()), n <= i,
            decreases ids.len() - i,
        {
            proof {
                lemma_count_push(ids@.subrange(0, i as int), ids@[i as int]);
                assert(ids@.subrange(0, i + 1) =~= ids@.subrange(0, i as int).push(ids@[i as int]));
            }
            if ids[i].change { n = n + 1; }
            i += 1;
        }
        assert(ids@.subrange(0, ids.len() as int) =~= ids@);
        n
    }

    fn insert_batch(&mut self, b: Batch)
        requires
            old(self).counted(), unique_seqs(old(self).v().batches),
            forall|j: int| 0 <= j < old(self).batches@.len() ==> (#[trigger] old(self).batches@[j]).seq != b.seq,
            b.changes == count(b.entries@, change()),
            old(self).changes as int + b.changes <= u64::MAX, old(self).foreign as int + b.entries@.len() <= u64::MAX,
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).batches@ == old(self).batches@.push(b),
            views(final(self).batches@) == views(old(self).batches@).push(bv(b)),
            unique_seqs(final(self).v().batches),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).own == old(self).own,
    {
        let ghost prev = *self;
        let n = self.foreign_in(&b.entries);
        proof { lemma_count_le(b.entries@, foreign(self.me)); }
        self.foreign = self.foreign + n;
        self.changes = self.changes + b.changes;
        let (seq, idle) = (b.seq, b.slot.is_none() && b.entries.len() > 0);
        self.batches.push(b);
        proof {
            lemma_views_push(prev.batches@, b);
            lemma_bsum_push(views(prev.batches@), bv(b));
            lemma_bmap_push(views(prev.batches@), bv(b));
        }
        if idle {
            self.unslotted_insert(seq);
        }
        proof {
            assert forall|j: int| 0 <= j < self.batches@.len() implies (#[trigger] self.batches@[j]).changes as int == count(self.batches@[j].entries@, change()) by {
                if j < prev.batches@.len() { assert(self.batches@[j] == prev.batches@[j]); }
            }
            assert forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s) by {
                if waits(prev.batches@, s) {
                    let i = choose|i: int| 0 <= i < prev.batches@.len() && (#[trigger] prev.batches@[i]).seq == s && prev.batches@[i].slot is None && prev.batches@[i].entries@.len() > 0;
                    assert(self.batches@[i] == prev.batches@[i]);
                }
                if s == seq && idle { assert(self.batches@[prev.batches@.len() as int].seq == s); }
                if waits(self.batches@, s) {
                    let i = choose|i: int| 0 <= i < self.batches@.len() && (#[trigger] self.batches@[i]).seq == s && self.batches@[i].slot is None && self.batches@[i].entries@.len() > 0;
                    if i < prev.batches@.len() { assert(self.batches@[i] == prev.batches@[i]); assert(waits(prev.batches@, s)); }
                }
            }
        }
    }

    fn remove_batch(&mut self, j: usize) -> (b: Batch)
        requires old(self).counted(), unique_seqs(old(self).v().batches), j < old(self).batches@.len(),
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            b == old(self).batches@[j as int],
            final(self).batches@ == old(self).batches@.remove(j as int),
            views(final(self).batches@) == views(old(self).batches@).remove(j as int),
            unique_seqs(final(self).v().batches),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).own == old(self).own,
    {
        let ghost prev = *self;
        proof {
            lemma_views_remove(prev.batches@, j as int);
            lemma_bsum_remove(views(prev.batches@), j as int);
            lemma_bmap_remove(views(prev.batches@), j as int);
            assert(views(prev.batches@)[j as int] == bv(prev.batches@[j as int]));
            assert(prev.batches@[j as int].changes as int == count(prev.batches@[j as int].entries@, change()));
        }
        let b = self.batches.remove(j);
        let n = self.foreign_in(&b.entries);
        self.foreign = self.foreign - n;
        self.changes = self.changes - b.changes;
        self.unslotted_remove(b.seq);
        proof {
            assert forall|i: int| 0 <= i < self.batches@.len() implies (#[trigger] self.batches@[i]).changes as int == count(self.batches@[i].entries@, change()) by {
                let i2 = if i < j { i } else { i + 1 };
                assert(self.batches@[i] == prev.batches@[i2]);
            }
            assert forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s) by {
                if waits(prev.batches@, s) && s != b.seq {
                    let i = choose|i: int| 0 <= i < prev.batches@.len() && (#[trigger] prev.batches@[i]).seq == s && prev.batches@[i].slot is None && prev.batches@[i].entries@.len() > 0;
                    assert(i != j);
                    let i2 = if i < j { i } else { i - 1 };
                    assert(self.batches@[i2] == prev.batches@[i]);
                }
                if waits(self.batches@, s) {
                    let i = choose|i: int| 0 <= i < self.batches@.len() && (#[trigger] self.batches@[i]).seq == s && self.batches@[i].slot is None && self.batches@[i].entries@.len() > 0;
                    let i2 = if i < j { i } else { i + 1 };
                    assert(self.batches@[i] == prev.batches@[i2]);
                    assert(waits(prev.batches@, s));
                    assert(views(prev.batches@)[i2].seq == s && views(prev.batches@)[j as int].seq == b.seq);
                    if s == b.seq { assert(i2 != j); }
                }
            }
        }
        b
    }

    fn set_own(&mut self, k: usize, o: Own)
        requires k < old(self).own@.len(), o.change == old(self).own@[k as int].change, old(self).counted(),
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).own@ == old(self).own@.update(k as int, o),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).batches == old(self).batches,
            final(self).unslotted == old(self).unslotted, final(self).changes == old(self).changes,
            final(self).foreign == old(self).foreign,
    {
        proof { lemma_ocount_update(self.own@, k as int, o); }
        self.own.set(k, o);
    }

    /// Own entry `id` was taken from `from` to `to`: the entry moves if `from` holds it, else
    /// what moved is a copy.
    fn moved(&mut self, id: u64, from: At, to: At)
        requires old(self).counted(), unique_own(old(self).own@),
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).batches == old(self).batches,
            final(self).unslotted == old(self).unslotted, final(self).foreign == old(self).foreign,
            final(self).changes == old(self).changes,
            final(self).own@.len() == old(self).own@.len(), unique_own(final(self).own@),
            forall|k: int| 0 <= k < old(self).own@.len() && (#[trigger] old(self).own@[k]).id != id ==> final(self).own@[k] == old(self).own@[k],
            forall|k: int| 0 <= k < old(self).own@.len() && (#[trigger] old(self).own@[k]).id == id ==> final(self).own@[k] == if same(old(self).own@[k].at, from) {
                Own { at: to, ..old(self).own@[k] }
            } else {
                Own { copied: true, ..old(self).own@[k] }
            },
    {
        match self.find_own(id) {
            Some(k) => {
                let mut o = self.own[k];
                if same_exec(o.at, from) { o.at = to } else { o.copied = true }
                self.set_own(k, o);
                proof {
                    assert forall|j: int| 0 <= j < old(self).own@.len() && (#[trigger] old(self).own@[j]).id == id implies j == k by {
                        if j < k { assert(old(self).own@[j].id != old(self).own@[k as int].id); }
                        if j > k { assert(old(self).own@[k as int].id != old(self).own@[j].id); }
                    }
                    assert forall|a: int, b: int| 0 <= a < b < self.own@.len() implies self.own@[a].id != self.own@[b].id by {
                        assert(old(self).own@[a].id != old(self).own@[b].id);
                    }
                }
            },
            None => {},
        }
    }

    // ---- transitions ----

    /// Batch `seq` is proposed in `slot`. DEV: whether it exists for the key and value.
    pub fn start(&mut self, seq: u64, slot: u64) -> (r: bool)
        requires old(self).inv_but(seq as int),
        ensures
            final(self).inv(), final(self).same_ids(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).v().own == old(self).v().own, held(final(self).v()) == held(old(self).v()),
            final(self).next_batch == old(self).next_batch, final(self).prune_due == old(self).prune_due,
            r ==> exists|j: int| 0 <= j < final(self).batches@.len() && (#[trigger] final(self).batches@[j]).seq == seq && final(self).batches@[j].slot == Some(slot),
            !r ==> final(self).inv_but(seq as int) && *final(self) == *old(self),
            (exists|j: int| 0 <= j < old(self).batches@.len() && (#[trigger] old(self).batches@[j]).seq == seq) ==> r,
    {
        let j = match self.find_batch(seq) {
            Some(j) => j,
            None => {
                proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
                return false;
            },
        };
        let ghost prev = *self;
        self.set_slot(j, Some(slot));
        self.unslotted_remove(seq);
        proof {
            lemma_e1_slotted(prev.v(), self.me, self.inc, j as int, slot);
            assert(self.v() =~= V { batches: prev.v().batches.update(j as int, BatchV { seq, entries: prev.v().batches[j as int].entries, slot: Some(slot) }), ..prev.v() });
            lemma_bsum_update(prev.v().batches, j as int, BatchV { seq, entries: prev.v().batches[j as int].entries, slot: Some(slot) });
            lemma_bmap_update(prev.v().batches, j as int, BatchV { seq, entries: prev.v().batches[j as int].entries, slot: Some(slot) });
            assert forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s) by {
                if waits(prev.batches@, s) && s != seq {
                    let i = choose|i: int| 0 <= i < prev.batches@.len() && (#[trigger] prev.batches@[i]).seq == s && prev.batches@[i].slot is None && prev.batches@[i].entries@.len() > 0;
                    assert(i != j);
                    assert(self.batches@[i] == prev.batches@[i]);
                }
                if waits(self.batches@, s) {
                    let i = choose|i: int| 0 <= i < self.batches@.len() && (#[trigger] self.batches@[i]).seq == s && self.batches@[i].slot is None && self.batches@[i].entries@.len() > 0;
                    assert(i != j);
                    assert(self.batches@[i] == prev.batches@[i]);
                    assert(waits(prev.batches@, s));
                    if s == seq {
                        assert(prev.v().batches[i].seq == prev.v().batches[j as int].seq);
                        lemma_one_seq(prev.v(), self.me, self.inc, i, j as int);
                    }
                }
            }
            assert forall|k: (u64, u64, u64)| holds(prev.v(), k) implies holds(self.v(), k) by {
                lemma_holds_frame(prev.v(), self.v(), k);
            }
            if !self.prune_due {
                assert forall|b: int| 0 <= b < self.batches@.len() && (#[trigger] self.batches@[b]).slot is None implies
                    exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])) by {
                    assert(b != j);
                    assert(self.batches@[b] == prev.batches@[b]);
                    if prev.batches@[b].seq == seq {
                        assert(prev.v().batches[b].seq == prev.v().batches[j as int].seq);
                        lemma_one_seq(prev.v(), self.me, self.inc, b, j as int);
                    }
                }
            }
            assert(self.batches@[j as int].seq == seq);
        }
        true
    }

    /// Fresh own ids: none held, awaited or committed.
    pub open spec fn fresh(self, id: u64) -> bool {
        !holds(self.v(), (self.me, self.inc, id)) && !has_own(self.v(), id) && !self.dedup@.contains((self.me, self.inc, id))
    }

    /// `propose` and the like queued an entry of this replica's (a fresh id).
    pub fn submit(&mut self, x: Item)
        requires old(self).calm(), x.origin == old(self).me, x.inc == old(self).inc, old(self).fresh(x.id),
        ensures
            final(self).inv(), final(self).settled(), final(self).frame(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()),
            forall|id: u64| has_own(final(self).v(), id) <==> has_own(old(self).v(), id) || id == x.id,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        proof {
            self.lemma_bounded();
            lemma_absent(prev.v(), me, inc, x.id);
        }
        let o = Own { id: x.id, change: x.change, hedged: false, at: At::Pending, copied: false };
        self.own.push(o);
        proof { lemma_ocount_push(prev.own@, o); }
        if x.change { self.changes = self.changes + 1; }
        self.push_pending(x);
        proof {
            let (v, w) = (prev.v(), self.v());
            lemma_count_push(v.pending, x);
            assert forall|i: int, j: int| 0 <= i < j < w.own.len() implies w.own[i].id != w.own[j].id by {
                if j == v.own.len() { assert(v.own[i].id != x.id); }
            }
            assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
                if k < v.own.len() {
                    let id = v.own[k].id;
                    assert(w.own[k] == v.own[k]);
                    assert(id != x.id);
                    assert(census(w, me, inc, id) == census(v, me, inc, id));
                    assert(fine(v, me, inc, v.own[k]));
                } else {
                    assert(w.own[k] == o);
                    let (c, c0) = (census(w, me, inc, x.id), census(v, me, inc, x.id));
                    assert(this(me, inc, x.id)(x));
                    assert(c.p == c0.p + 1 && c.r == c0.r && c.f == c0.f && c.b == c0.b);
                }
            }
            assert(waits(self.batches@, 0) == waits(prev.batches@, 0));
            assert forall|id: u64| has_own(w, id) <==> has_own(v, id) || id == x.id by {
                if has_own(v, id) { let k = choose|k: int| 0 <= k < v.own.len() && v.own[k].id == id; assert(w.own[k] == v.own[k]); }
                if id == x.id { assert(w.own[v.own.len() as int].id == id); }
                if has_own(w, id) && id != x.id { let k = choose|k: int| 0 <= k < w.own.len() && w.own[k].id == id; assert(v.own[k] == w.own[k]); }
            }
            assert forall|k: (u64, u64, u64)| holds(v, k) implies holds(w, k) by {
                if exists|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k {
                    let i = choose|i: int| 0 <= i < v.pending.len() && key(#[trigger] v.pending[i]) == k;
                    assert(w.pending[i] == v.pending[i]);
                }
            }
        }
    }

    /// `forwarded.insert(id, (e, now))`.
    fn fwd_insert(&mut self, e: Item, now: u64)
        requires
            e.origin == old(self).me && e.inc == old(self).inc, unique_ids(old(self).forwarded@),
            forall|j: int| 0 <= j < old(self).forwarded@.len() ==> (#[trigger] old(self).forwarded@[j]).0.origin == old(self).me && old(self).forwarded@[j].0.inc == old(self).inc,
        ensures
            unique_ids(final(self).forwarded@), fmap(final(self).forwarded@) == fmap(old(self).forwarded@).insert(e.id, now),
            forall|j: int| 0 <= j < final(self).forwarded@.len() ==> (#[trigger] final(self).forwarded@[j]).0.origin == final(self).me && final(self).forwarded@[j].0.inc == final(self).inc,
            forall|j: int| 0 <= j < old(self).forwarded@.len() ==> fwd_has(final(self).forwarded@, key((#[trigger] old(self).forwarded@[j]).0)),
            fwd_has(final(self).forwarded@, key(e)),
            final(self).forwarded@.len() <= old(self).forwarded@.len() + 1,
            final(self).frame(*old(self)), final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).batches == old(self).batches, final(self).own == old(self).own, final(self).unslotted == old(self).unslotted,
            final(self).foreign == old(self).foreign, final(self).changes == old(self).changes,
    {
        let ghost f = self.forwarded@;
        match self.find_fwd(e.id) {
            Some(j) => {
                proof { lemma_fmap_set(f, j as int, (e, now)); }
                self.forwarded.set(j, (e, now));
                assert forall|a: int| 0 <= a < f.len() implies fwd_has(self.forwarded@, key((#[trigger] f[a]).0)) by {
                    if a != j { assert(self.forwarded@[a] == f[a]); } else { assert(key(self.forwarded@[a].0) == key(f[a].0)); }
                }
                assert(key(self.forwarded@[j as int].0) == key(e));
            },
            None => {
                proof { lemma_fmap_push(f, (e, now)); }
                self.forwarded.push((e, now));
                assert forall|a: int| 0 <= a < f.len() implies fwd_has(self.forwarded@, key((#[trigger] f[a]).0)) by {
                    assert(self.forwarded@[a] == f[a]);
                }
                assert(key(self.forwarded@[f.len() as int].0) == key(e));
            },
        }
    }

    /// Hands everything pending to the target unless this replica proposes it `direct`ly:
    /// own entries are tracked from `now`. Returns the entries to send.
    pub fn route(&mut self, direct: bool, now: u64) -> (out: Vec<Item>)
        requires old(self).calm(),
        ensures
            final(self).inv(), final(self).settled(), final(self).frame(*old(self)),
            final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), out@),
            // E5: nothing pending unless proposed directly.
            !direct ==> final(self).pending@.len() == 0,
            // E3: handed to the target only, each once.
            out@ == if direct { Seq::empty() } else { old(self).pending@ },
            direct ==> final(self).pending == old(self).pending,
            held(final(self).v()) <= held(old(self).v()),
    {
        if direct || self.pending.len() == 0 {
            proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
            return Vec::new();
        }
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        // DEV: a swap for `drain(..).collect()`.
        let mut entries = Vec::new();
        std::mem::swap(&mut self.pending, &mut entries);
        let n = Self::changes_in(&entries);
        self.changes = self.changes - n;
        proof {
            assert(prev.v().pending == entries@);
            assert(self.v() == V { pending: Seq::empty(), ..prev.v() });
            assert(count(Seq::<Item>::empty(), change()) == 0);
            assert(entries@.subrange(0, entries@.len() as int) =~= entries@);
            assert(V { pending: entries@.subrange(0, entries@.len() as int), ..self.v() } == prev.v());
        }
        let mut i = 0;
        while i < entries.len()
            invariant
                i <= entries@.len(), entries@ == prev.pending@, self.me == me, self.inc == inc,
                self.frame(prev), self.pending@.len() == 0, self.counted(),
                self.relayed == prev.relayed, self.batches == prev.batches, self.unslotted == prev.unslotted,
                shape(V { pending: entries@.subrange(i as int, entries@.len() as int), ..self.v() }, me, inc),
                e1(V { pending: entries@.subrange(i as int, entries@.len() as int), ..self.v() }, me, inc),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                forall|j: int| 0 <= j < prev.forwarded@.len() ==> fwd_has(self.forwarded@, key((#[trigger] prev.forwarded@[j]).0)),
                self.forwarded@.len() <= prev.forwarded@.len() + i,
            decreases entries.len() - i,
        {
            let e = entries[i];
            let ghost v0 = V { pending: entries@.subrange(i as int, entries@.len() as int), ..self.v() };
            proof {
                lemma_count_pop(v0.pending);
                assert(v0.pending.subrange(1, v0.pending.len() as int) =~= entries@.subrange(i + 1, entries@.len() as int));
            }
            if e.origin == self.me && e.inc == self.inc {
                let ghost flen = self.forwarded@.len();
                self.fwd_insert(e, now);
                proof { lemma_fmap(self.forwarded@); lemma_fmap(v0.fwd); }
                let ghost v1 = V { pending: entries@.subrange(i + 1, entries@.len() as int), ..self.v() };
                proof {
                    assert forall|id: u64| id != e.id implies #[trigger] census(v1, me, inc, id) == census(v0, me, inc, id) by {}
                }
                match self.find_own(e.id) {
                    Some(k) => {
                        let mut o = self.own[k];
                        let ghost o0 = o;
                        match o.at {
                            At::Pending => o.at = At::Forwarded { since: now },
                            At::Forwarded { .. } => {
                                o.at = At::Forwarded { since: now };
                                o.copied = true;
                            },
                            _ => o.copied = true,
                        }
                        self.set_own(k, o);
                        proof {
                            let v2 = V { pending: entries@.subrange(i + 1, entries@.len() as int), ..self.v() };
                            assert(fine(v0, me, inc, o0));
                            let (c0, c2) = (census(v0, me, inc, e.id), census(v2, me, inc, e.id));
                            assert(c2.p + 1 == c0.p && c2.r == c0.r && c2.b == c0.b && c2.f == Some(now));
                            assert forall|j: int| 0 <= j < v0.own.len() && (#[trigger] v0.own[j]).id == e.id implies v2.own[j].id == e.id && fine(v2, me, inc, v2.own[j]) by {
                                if j != k { assert(v0.own[j].id != v0.own[k as int].id); }
                            }
                            lemma_e1_step(v0, v2, me, inc, e.id);
                        }
                    },
                    None => {
                        proof {
                            assert forall|j: int| 0 <= j < v0.own.len() && (#[trigger] v0.own[j]).id == e.id implies v1.own[j].id == e.id && fine(v1, me, inc, v1.own[j]) by {}
                            lemma_e1_step(v0, v1, me, inc, e.id);
                        }
                    },
                }
            } else {
                proof {
                    let v1 = V { pending: entries@.subrange(i + 1, entries@.len() as int), ..self.v() };
                    assert forall|id: u64| #[trigger] census(v1, me, inc, id) == census(v0, me, inc, id) by {}
                    lemma_e1_step(v0, v1, me, inc, e.id);
                }
            }
            i += 1;
        }
        proof {
            let w = self.v();
            assert(self.pending@ =~= Seq::<Item>::empty());
            assert(entries@.subrange(entries@.len() as int, entries@.len() as int) =~= Seq::<Item>::empty());
            assert(w == V { pending: entries@.subrange(i as int, entries@.len() as int), ..w });
            assert forall|k: (u64, u64, u64)| holds(prev.v(), k) implies holds(w, k) || self.dedup@.contains(k) || exists|i: int| 0 <= i < entries@.len() && key(#[trigger] entries@[i]) == k by {
                let v = prev.v();
                if exists|a: int| 0 <= a < v.fwd.len() && key((#[trigger] v.fwd[a]).0) == k {
                    let a = choose|a: int| 0 <= a < v.fwd.len() && key((#[trigger] v.fwd[a]).0) == k;
                    assert(fwd_has(self.forwarded@, key(prev.forwarded@[a].0)));
                    let b = choose|b: int| 0 <= b < self.forwarded@.len() && key((#[trigger] self.forwarded@[b]).0) == key(prev.forwarded@[a].0);
                    assert(key(w.fwd[b].0) == k);
                }
                if exists|j: int, a: int| 0 <= j < v.batches.len() && 0 <= a < v.batches[j].entries.len() && key(#[trigger] v.batches[j].entries[a]) == k {
                    let (j, a) = choose|j: int, a: int| 0 <= j < v.batches.len() && 0 <= a < v.batches[j].entries.len() && key(#[trigger] v.batches[j].entries[a]) == k;
                    assert(w.batches[j].entries[a] == v.batches[j].entries[a]);
                }
            }
            assert forall|id: u64| has_own(w, id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < w.own.len() && w.own[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(w, id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(w.own[k].id == id);
            }
            assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                assert(prev.own@[k].id == self.own@[k].id);
            }
        }
        entries
    }

    /// A follower handed entries to this replica: into `pending` (to hand on, or as the
    /// leader) or `relayed` (as a deputy), unless that queue would exceed `cap`. An own entry
    /// among them is a copy.
    pub fn received(&mut self, entries: Vec<Item>, to_pending: bool, cap: usize) -> (r: bool)
        requires old(self).calm(), entries@.len() <= held_max(),
        ensures
            final(self).inv(), final(self).settled(), final(self).frame(*old(self)),
            final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            !r ==> *final(self) == *old(self),
            r && to_pending ==> final(self).pending@ == old(self).pending@ + entries@ && final(self).relayed == old(self).relayed,
            r && !to_pending ==> final(self).relayed@ == old(self).relayed@ + entries@ && final(self).pending == old(self).pending,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let queued = if to_pending { self.pending.len() } else { self.relayed.len() };
        // DEV: without overflow, `queued + entries.len() > cap`.
        if queued > cap || entries.len() > cap - queued {
            proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
            return false; // the origin hedges
        }
        let mut i = 0;
        while i < entries.len()
            invariant
                i <= entries@.len(), self.me == me, self.inc == inc, self.frame(prev), self.counted(),
                self.pending == prev.pending, self.relayed == prev.relayed, self.forwarded == prev.forwarded,
                self.batches == prev.batches, self.unslotted == prev.unslotted, self.foreign == prev.foreign,
                self.own@.len() == prev.own@.len(), unique_own(prev.own@),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id
                    && self.own@[k].at == prev.own@[k].at && self.own@[k].change == prev.own@[k].change
                    && self.own@[k].hedged == prev.own@[k].hedged && (prev.own@[k].copied ==> self.own@[k].copied),
                forall|k: int, a: int| 0 <= k < self.own@.len() && 0 <= a < i && this(me, inc, (#[trigger] self.own@[k]).id)(#[trigger] entries@[a]) ==> self.own@[k].copied,
            decreases entries.len() - i,
        {
            let e = entries[i];
            if e.origin == self.me && e.inc == self.inc {
                let ghost before = self.own@;
                match self.find_own(e.id) {
                    Some(k) => {
                        let mut o = self.own[k];
                        o.copied = true;
                        self.set_own(k, o);
                        proof {
                            assert forall|j: int, a: int| 0 <= j < self.own@.len() && 0 <= a < i + 1 && this(me, inc, (#[trigger] self.own@[j]).id)(#[trigger] entries@[a]) implies self.own@[j].copied by {
                                if j != k && a == i {
                                    assert(before[j].id == e.id);
                                    if j < k { assert(prev.own@[j].id != prev.own@[k as int].id); }
                                    if j > k { assert(prev.own@[k as int].id != prev.own@[j].id); }
                                }
                            }
                        }
                    },
                    None => {},
                }
            }
            i += 1;
        }
        let n = Self::changes_in(&entries);
        let ghost mid = *self;
        proof {
            self.lemma_bounded();
            lemma_count_le(entries@, change());
            lemma_count_le(entries@, foreign(me));
            assert(held(self.v()) == held(prev.v()));
        }
        self.changes = self.changes + n;
        let mut entries = entries;
        let ghost es = entries@;
        if to_pending {
            self.pending.append(&mut entries);
            proof { lemma_count_add(mid.pending@, es, change()); }
        } else {
            let f = self.foreign_in(&entries);
            self.foreign = self.foreign + f;
            self.relayed.append(&mut entries);
            proof {
                lemma_count_add(mid.relayed@, es, change());
                lemma_count_add(mid.relayed@, es, foreign(me));
            }
        }
        proof {
            let (v, m, w) = (prev.v(), mid.v(), self.v());
            assert(unique_own(w.own)) by {
                assert forall|a: int, b: int| 0 <= a < b < w.own.len() implies w.own[a].id != w.own[b].id by {
                    assert(v.own[a].id != v.own[b].id);
                }
            }
            assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
                let o = w.own[k];
                let x = this(me, inc, o.id);
                assert(fine(v, me, inc, v.own[k]));
                assert(fine(v, me, inc, o));
                assert(census(m, me, inc, o.id) == census(v, me, inc, o.id));
                lemma_count_add(m.pending, es, x);
                lemma_count_add(m.relayed, es, x);
                if exists|a: int| 0 <= a < es.len() && x(#[trigger] es[a]) {
                    let a = choose|a: int| 0 <= a < es.len() && x(#[trigger] es[a]);
                    assert(o.copied);
                    lemma_fine_more(v, w, me, inc, o);
                } else {
                    lemma_count_none(es, x);
                    assert(census(w, me, inc, o.id) == census(v, me, inc, o.id));
                }
            }
            assert forall|k: (u64, u64, u64)| holds(v, k) implies holds(w, k) by {
                if exists|a: int| 0 <= a < v.pending.len() && key(#[trigger] v.pending[a]) == k {
                    let a = choose|a: int| 0 <= a < v.pending.len() && key(#[trigger] v.pending[a]) == k;
                    assert(w.pending[a] == v.pending[a]);
                }
                if exists|a: int| 0 <= a < v.relayed.len() && key(#[trigger] v.relayed[a]) == k {
                    let a = choose|a: int| 0 <= a < v.relayed.len() && key(#[trigger] v.relayed[a]) == k;
                    assert(w.relayed[a] == v.relayed[a]);
                }
                lemma_holds_frame(v, V { pending: v.pending, relayed: v.relayed, ..w }, k);
            }
            assert forall|id: u64| has_own(w, id) implies has_own(v, id) by {
                let k = choose|k: int| 0 <= k < w.own.len() && w.own[k].id == id;
                assert(v.own[k].id == id);
            }
            assert forall|id: u64| has_own(v, id) implies has_own(w, id) by {
                let k = choose|k: int| 0 <= k < v.own.len() && v.own[k].id == id;
                assert(w.own[k].id == id);
            }
            assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                assert(prev.own@[k].id == self.own@[k].id);
            }
            lemma_ocount_same(prev.own@, self.own@);
        }
        true
    }

    /// `forwarded.remove(&id)`.
    fn fwd_drop(&mut self, id: u64)
        requires shape(old(self).v(), old(self).me, old(self).inc),
        ensures
            shape(final(self).v(), final(self).me, final(self).inc),
            fmap(final(self).forwarded@) == fmap(old(self).forwarded@).remove(id),
            final(self).forwarded@.len() <= old(self).forwarded@.len(),
            forall|k: (u64, u64, u64)| holds(old(self).v(), k) ==> holds(final(self).v(), k) || k == (old(self).me, old(self).inc, id),
            final(self).frame(*old(self)), final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).batches == old(self).batches, final(self).own == old(self).own, final(self).unslotted == old(self).unslotted,
            final(self).foreign == old(self).foreign, final(self).changes == old(self).changes,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        if let Some(j) = self.find_fwd(id) {
            proof {
                lemma_fmap_remove(self.forwarded@, j as int);
                assert forall|k: (u64, u64, u64)| holds(prev.v(), k) implies holds(V { fwd: prev.forwarded@.remove(j as int), ..prev.v() }, k) || k == (me, inc, id) by {
                    lemma_holds_fwd_remove(prev.v(), j as int, k);
                }
            }
            self.forwarded.remove(j);
            proof {
                assert(self.v() == V { fwd: prev.forwarded@.remove(j as int), ..prev.v() });
                assert forall|a: int| 0 <= a < self.forwarded@.len() implies (#[trigger] self.forwarded@[a]).0.origin == me && self.forwarded@[a].0.inc == inc by {
                    let a2 = if a < j { a } else { a + 1 };
                    assert(self.forwarded@[a] == prev.forwarded@[a2]);
                }
            }
        } else {
            proof {
                lemma_fmap(prev.forwarded@);
                assert(fmap(prev.forwarded@).remove(id) =~= fmap(prev.forwarded@));
                assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) by {}
            }
        }
    }

    /// An own entry is resolved (committed, superseded, or internal): no longer handed to
    /// anyone nor awaited. DEV: the `Left` is the record.
    #[verifier::rlimit(150)]
    pub fn resolved(&mut self, id: u64) -> (r: Option<Own>)
        requires old(self).inv(), old(self).dedup@.contains((old(self).me, old(self).inc, id)),
        ensures
            final(self).inv(), final(self).frame(*old(self)),
            final(self).kept(*old(self), set![id], Seq::empty()),
            forall|i: u64| has_own(final(self).v(), i) <==> has_own(old(self).v(), i) && i != id,
            r is Some <==> has_own(old(self).v(), id),
            r is None ==> final(self).own == old(self).own,
            r matches Some(o) ==> o.id == id,
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).batches == old(self).batches,
            final(self).own@.len() <= old(self).own@.len(), final(self).forwarded@.len() <= old(self).forwarded@.len(),
            forall|k: int| 0 <= k < final(self).own@.len() ==> exists|j: int| 0 <= j < old(self).own@.len() && #[trigger] final(self).own@[k] == old(self).own@[j],
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        self.fwd_drop(id);
        let ghost mid = *self;
        proof { lemma_others_fine(prev.v(), mid.v(), me, inc, id); }
        match self.find_own(id) {
            Some(k) => {
                proof {
                    let m = mid.v();
                    assert forall|j: int| 0 <= j < m.own.len() && j != k implies fine(m, me, inc, #[trigger] m.own[j]) by {
                        if j < k { assert(m.own[j].id != m.own[k as int].id); } else { assert(m.own[k as int].id != m.own[j].id); }
                    }
                    lemma_own_remove(m, me, inc, k as int);
                }
                let o = self.own_drop(k);
                proof {
                    let (v, m, w) = (prev.v(), mid.v(), self.v());
                    assert(w == V { own: m.own.remove(k as int), ..m });
                    assert forall|k2: (u64, u64, u64)| holds(m, k2) implies holds(w, k2) by {
                        lemma_holds_frame(m, w, k2);
                    }
                    assert(has_own(v, id)) by { assert(v.own[k as int].id == id); }
                    assert forall|a: int| 0 <= a < w.own.len() implies exists|j: int| 0 <= j < v.own.len() && #[trigger] w.own[a] == v.own[j] by {
                        let a2 = if a < k { a } else { a + 1 };
                        assert(w.own[a] == v.own[a2]);
                    }
                }
                Some(o)
            },
            None => {
                proof {
                    let (v, w) = (prev.v(), self.v());
                    assert forall|a: int| 0 <= a < w.own.len() implies exists|j: int| 0 <= j < v.own.len() && #[trigger] w.own[a] == v.own[j] by {
                        assert(w.own[a] == v.own[a]);
                    }
                }
                None
            },
        }
    }

    fn own_drop(&mut self, k: usize) -> (o: Own)
        requires old(self).counted(), k < old(self).own@.len(),
        ensures
            final(self).counted(), final(self).frame(*old(self)), o == old(self).own@[k as int],
            final(self).own@ == old(self).own@.remove(k as int),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).batches == old(self).batches,
            final(self).unslotted == old(self).unslotted, final(self).foreign == old(self).foreign,
    {
        proof { lemma_ocount_remove(self.own@, k as int); }
        let o = self.own.remove(k);
        if o.change { self.changes = self.changes - 1; }
        o
    }

    /// Own batch `seq` was decided and applied: its entries are all committed.
    pub fn applied(&mut self, seq: u64)
        requires
            old(self).inv(), old(self).settled(),
            forall|j: int, i: int| 0 <= j < old(self).batches@.len() && (#[trigger] old(self).batches@[j]).seq == seq && 0 <= i < old(self).batches@[j].entries@.len() ==>
                old(self).dedup@.contains(key(#[trigger] old(self).batches@[j].entries@[i])),
        ensures
            final(self).inv(), final(self).settled(), final(self).frame(*old(self)),
            final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).own == old(self).own,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        if let Some(j) = self.find_batch(seq) {
            let _ = self.remove_batch(j);
            proof {
                let (v, w) = (prev.v(), self.v());
                assert forall|i: int| 0 <= i < v.batches[j as int].entries.len() implies self.dedup@.contains(key(#[trigger] v.batches[j as int].entries[i])) by {
                    assert(v.batches[j as int].entries[i] == prev.batches@[j as int].entries@[i]);
                }
                lemma_e1_drop(v, me, inc, j as int, self.dedup@);
                assert(w == V { batches: v.batches.remove(j as int), ..v });
                self.lemma_due_remove(prev, j as int);
                assert forall|k: (u64, u64, u64)| holds(v, k) implies holds(w, k) || self.dedup@.contains(k) by {
                    lemma_holds_drop(v, j as int, k);
                }
                assert forall|b: int| 0 <= b < self.batches@.len() implies (#[trigger] self.batches@[b]).seq < self.next_batch by {
                    let b2 = if b < j { b } else { b + 1 };
                    assert(self.batches@[b] == prev.batches@[b2]);
                }
            }
        } else {
            proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
        }
    }

    /// `due` survives removing a batch.
    proof fn lemma_due_remove(self, prev: Entries, j: int)
        requires
            prev.due(-1), 0 <= j < prev.batches@.len(), self.batches@ == prev.batches@.remove(j),
            self.prune_due == prev.prune_due, self.dedup == prev.dedup,
        ensures self.due(-1),
    {
        assert forall|b: int| 0 <= b < self.batches@.len() && (#[trigger] self.batches@[b]).slot is None && self.batches@[b].seq != -1 && !self.prune_due implies
            exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])) by {
            let b2 = if b < j { b } else { b + 1 };
            assert(self.batches@[b] == prev.batches@[b2]);
        }
    }

    /// Entries committed: a batch waiting for a slot may hold nothing else. DEV: the new
    /// dedup state is passed in.
    pub fn dedup_grew(&mut self, dedup: &Dedup)
        requires old(self).inv(), forall|k| old(self).dedup@.contains(k) ==> dedup.has(k),
        ensures
            final(self).inv(), final(self).me == old(self).me, final(self).inc == old(self).inc,
            forall|k| final(self).dedup@.contains(k) <==> dedup.has(k),
            final(self).v() == old(self).v(), final(self).next_batch == old(self).next_batch,
    {
        let ghost prev = *self;
        proof { self.dedup = Ghost(dedup.keys@.to_set()); }
        self.prune_due = self.prune_due || self.unslotted.len() > 0;
        proof {
            if !self.prune_due {
                assert forall|b: int| 0 <= b < self.batches@.len() implies (#[trigger] self.batches@[b]).slot is Some by {
                    if self.batches@[b].slot is None {
                        let i = choose|i: int| 0 <= i < prev.batches@[b].entries@.len() && !prev.dedup@.contains(key(#[trigger] prev.batches@[b].entries@[i]));
                        assert(waits(self.batches@, self.batches@[b].seq));
                        assert(self.unslotted@.contains(self.batches@[b].seq));
                    }
                }
            }
        }
    }

    /// Whether every entry is in dedup. DEV: an index loop for `!any(..)`.
    fn all_in(entries: &Vec<Item>, dedup: &Dedup) -> (r: bool)
        ensures r == forall|i: int| 0 <= i < entries@.len() ==> dedup.has(key(#[trigger] entries@[i])),
    {
        let mut i = 0;
        while i < entries.len()
            invariant i <= entries@.len(), forall|a: int| 0 <= a < i ==> dedup.has(key(#[trigger] entries@[a])),
            decreases entries.len() - i,
        {
            if !dedup.contains(entries[i].origin, entries[i].inc, entries[i].id) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Batches whose entries all committed elsewhere are dead weight. DEV: removed in place,
    /// not collected first (whether a batch is dead does not depend on the others).
    pub fn prune(&mut self, dedup: &Dedup)
        requires old(self).ready(*dedup),
        ensures
            final(self).ready(*dedup), final(self).frame_but_prune(*old(self)),
            final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).own == old(self).own, final(self).forwarded == old(self).forwarded,
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            held(final(self).v()) <= held(old(self).v()),
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        if !self.prune_due {
            proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
            return;
        }
        self.prune_due = false;
        let mut j = 0;
        while j < self.batches.len()
            invariant
                j <= self.batches@.len(), self.me == me, self.inc == inc, self.dedup == prev.dedup,
                self.next_batch == prev.next_batch, !self.prune_due, self.counted(),
                shape(self.v(), me, inc), e1(self.v(), me, inc), self.own == prev.own,
                self.pending == prev.pending, self.relayed == prev.relayed, self.forwarded == prev.forwarded,
                unresolved(self.own@, self.dedup@, me, inc), forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                forall|b: int| 0 <= b < self.batches@.len() ==> (#[trigger] self.batches@[b]).seq < self.next_batch,
                forall|b: int| 0 <= b < j && (#[trigger] self.batches@[b]).slot is None ==>
                    exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])),
                forall|k: (u64, u64, u64)| holds(prev.v(), k) ==> holds(self.v(), k) || self.dedup@.contains(k),
                held(self.v()) <= held(prev.v()),
            decreases self.batches.len() - j,
        {
            let dead = self.batches[j].slot.is_none() && Self::all_in(&self.batches[j].entries, dedup);
            if dead {
                let ghost before = *self;
                let _ = self.remove_batch(j);
                proof {
                    let (v, w) = (before.v(), self.v());
                    assert forall|i: int| 0 <= i < v.batches[j as int].entries.len() implies self.dedup@.contains(key(#[trigger] v.batches[j as int].entries[i])) by {
                        assert(v.batches[j as int].entries[i] == before.batches@[j as int].entries@[i]);
                    }
                    lemma_e1_drop(v, me, inc, j as int, self.dedup@);
                    assert(w == V { batches: v.batches.remove(j as int), ..v });
                    assert forall|b: int| 0 <= b < j && (#[trigger] self.batches@[b]).slot is None implies
                        exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])) by {
                        assert(self.batches@[b] == before.batches@[b]);
                    }
                    assert forall|b: int| 0 <= b < self.batches@.len() implies (#[trigger] self.batches@[b]).seq < self.next_batch by {
                        let b2 = if b < j { b } else { b + 1 };
                        assert(self.batches@[b] == before.batches@[b2]);
                    }
                    assert forall|k: (u64, u64, u64)| holds(prev.v(), k) implies holds(w, k) || self.dedup@.contains(k) by {
                        if holds(v, k) {
                            lemma_holds_drop(v, j as int, k);
                        }
                    }
                    lemma_bsum_remove(v.batches, j as int);
                }
            } else {
                proof {
                    if self.batches@[j as int].slot is None {
                        let b = self.batches@[j as int];
                        let i = choose|i: int| 0 <= i < b.entries@.len() && !dedup.has(key(#[trigger] b.entries@[i]));
                        assert(!self.dedup@.contains(key(b.entries@[i])));
                    }
                }
                j += 1;
            }
        }
        proof {
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {}
        }
    }

    /// What `prune` keeps.
    pub open spec fn frame_but_prune(self, prev: Entries) -> bool {
        &&& self.me == prev.me && self.inc == prev.inc && self.dedup == prev.dedup
        &&& self.next_batch == prev.next_batch
    }

    /// The state with a new batch of `es`, not yet in a slot, under the next seq.
    pub open spec fn vnew(self, es: Seq<Item>) -> V {
        V { batches: views(self.batches@).push(BatchV { seq: self.next_batch, entries: es, slot: None }), ..self.v() }
    }

    /// A new local batch of `entries`, not yet in a slot.
    pub fn new_batch(&mut self, entries: Vec<Item>) -> (seq: u64)
        requires
            old(self).counted(), shape(old(self).v(), old(self).me, old(self).inc), old(self).next_batch < u64::MAX,
            forall|j: int| 0 <= j < old(self).batches@.len() ==> (#[trigger] old(self).batches@[j]).seq < old(self).next_batch,
            e1(old(self).vnew(entries@), old(self).me, old(self).inc),
            held(old(self).v()) + entries@.len() <= 2 * held_max(),
        ensures
            seq == old(self).next_batch, final(self).next_batch == old(self).next_batch + 1,
            final(self).v() == old(self).vnew(entries@),
            shape(final(self).v(), final(self).me, final(self).inc), e1(final(self).v(), final(self).me, final(self).inc),
            final(self).counted(),
            forall|j: int| 0 <= j < final(self).batches@.len() ==> (#[trigger] final(self).batches@[j]).seq < final(self).next_batch,
            final(self).batches@.len() == old(self).batches@.len() + 1,
            forall|j: int| 0 <= j < old(self).batches@.len() ==> #[trigger] final(self).batches@[j] == old(self).batches@[j],
            final(self).batches@.last().seq == seq && final(self).batches@.last().slot is None && final(self).batches@.last().entries@ == entries@,
            final(self).me == old(self).me, final(self).inc == old(self).inc, final(self).dedup == old(self).dedup,
            final(self).prune_due == old(self).prune_due,
            final(self).forwarded == old(self).forwarded, final(self).pending == old(self).pending,
            final(self).relayed == old(self).relayed, final(self).own == old(self).own,
    {
        let ghost prev = *self;
        let seq = self.next_batch;
        self.next_batch = seq + 1;
        let changes = Self::changes_in(&entries);
        proof {
            self.lemma_bounded();
            lemma_count_le(entries@, change());
            assert forall|j: int| 0 <= j < self.batches@.len() implies (#[trigger] self.batches@[j]).seq != seq by {}
        }
        let ghost es = entries@;
        let b = Batch { seq, entries, slot: None, changes, handed: false };
        self.insert_batch(b);
        proof {
            assert(bv(b) == BatchV { seq, entries: es, slot: None });
            assert(self.v() == prev.vnew(es));
            assert forall|j: int| 0 <= j < self.batches@.len() implies (#[trigger] self.batches@[j]).seq < self.next_batch by {
                if j < prev.batches@.len() { assert(self.batches@[j] == prev.batches@[j]); }
            }
        }
        seq
    }

    /// The new batch `cut` fills, counted as a batch.
    pub open spec fn vcut(self, taken: Seq<Item>) -> V { self.vnew(taken) }

    /// Cuts the next batch: relayed entries always, own pending ones only when proposing them
    /// directly (`lead`). DEV: at most `max` entries for `max_bytes`.
    #[verifier::rlimit(100)]
    pub fn cut(&mut self, lead: bool, max: usize, dedup: &Dedup) -> (r: Option<u64>)
        requires old(self).ready(*dedup), old(self).next_batch < u64::MAX,
        ensures
            final(self).ready(*dedup), final(self).same_ids(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).forwarded == old(self).forwarded, held(final(self).v()) <= held(old(self).v()),
            r matches Some(s) ==> s == old(self).next_batch && waits(final(self).batches@, s),
            r is None ==> final(self).next_batch == old(self).next_batch,
            r is Some ==> final(self).next_batch == old(self).next_batch + 1,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let seq = self.next_batch;
        let mut taken: Vec<Item> = Vec::new();
        proof {
            assert forall|k: int| 0 <= k < prev.own@.len() implies count(Seq::<Item>::empty(), this(me, inc, (#[trigger] prev.own@[k]).id)) == 0 by {}
            lemma_e1_add_batch(prev.v(), me, inc, BatchV { seq, entries: Seq::empty(), slot: None });
            lemma_bsum_push(views(prev.batches@), BatchV { seq, entries: Seq::empty(), slot: None });
            assert forall|k| holds(prev.v(), k) implies holds(self.vcut(taken@), k) by {
                lemma_holds_prefix(prev.v(), self.vcut(taken@), k);
            }
        }
        loop
            invariant
                self.me == me, self.inc == inc, self.frame(prev), self.counted(), seq == self.next_batch,
                self.batches == prev.batches, self.forwarded == prev.forwarded, self.unslotted == prev.unslotted,
                unique_seqs(views(self.batches@)),
                forall|j: int| 0 <= j < self.batches@.len() ==> (#[trigger] self.batches@[j]).seq < seq,
                shape(self.vcut(taken@), me, inc), e1(self.vcut(taken@), me, inc),
                self.own@.len() == prev.own@.len(), unresolved(self.own@, self.dedup@, me, inc),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                forall|i: int| 0 <= i < taken@.len() ==> !self.dedup@.contains(key(#[trigger] taken@[i])),
                forall|k: (u64, u64, u64)| holds(prev.v(), k) ==> holds(self.vcut(taken@), k) || self.dedup@.contains(k),
                held(self.v()) + taken@.len() <= held(prev.v()), held(prev.v()) <= held_max(),
            decreases self.relayed@.len() + self.pending@.len(),
        {
            let relayed = if self.relayed.len() > 0 {
                true
            } else if lead && self.pending.len() > 0 {
                false
            } else {
                break;
            };
            if taken.len() >= max {
                break;
            }
            let ghost before = *self;
            let ghost bt = taken@;
            let e = match self.pop(relayed) {
                Some(e) => e,
                None => break,
            };
            let ghost v0 = before.vcut(bt);
            let ghost x = this(me, inc, e.id);
            proof {
                lemma_count_pop(if relayed { before.relayed@ } else { before.pending@ });
                lemma_bmap_push(views(before.batches@), BatchV { seq, entries: bt, slot: None });
            }
            if dedup.contains(e.origin, e.inc, e.id) {
                proof {
                    let v1 = self.vcut(taken@);
                    assert forall|i: u64| #[trigger] census(v1, me, inc, i) == census(v0, me, inc, i) || (e.origin == me && e.inc == inc && i == e.id) by {}
                    assert forall|k: int| 0 <= k < v0.own.len() implies (#[trigger] v0.own[k]).id != e.id || !(e.origin == me && e.inc == inc) by {
                        assert(!self.dedup@.contains((me, inc, v0.own[k].id)));
                    }
                    assert forall|k: int| 0 <= k < v0.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                        assert(fine(v0, me, inc, v0.own[k]));
                        assert(census(v1, me, inc, v1.own[k].id) == census(v0, me, inc, v1.own[k].id));
                    }
                    assert forall|k| holds(prev.v(), k) implies holds(v1, k) || self.dedup@.contains(k) by {
                        if holds(v0, k) { lemma_holds_pop(v0, v1, relayed, k); }
                    }
                    lemma_bsum_push(views(self.batches@), BatchV { seq, entries: taken@, slot: None });
                }
                continue;
            }
            if e.origin == self.me && e.inc == self.inc {
                self.moved(e.id, if relayed { At::Relayed } else { At::Pending }, At::Batched { seq });
                proof {
                    assert forall|k: int| 0 <= k < self.own@.len() implies (#[trigger] self.own@[k]).id == before.own@[k].id by {
                        if before.own@[k].id == e.id {} else {}
                    }
                }
            }
            let ghost mv = *self;
            taken.push(e);
            proof {
                let v1 = self.vcut(taken@);
                lemma_count_push(bt, e);
                lemma_bmap_push(views(self.batches@), BatchV { seq, entries: taken@, slot: None });
                assert forall|i: u64| i != e.id || !(e.origin == me && e.inc == inc) implies #[trigger] census(v1, me, inc, i) == census(v0, me, inc, i) by {}
                assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                    let o0 = v0.own[k];
                    assert(fine(v0, me, inc, o0));
                    if o0.id == e.id && e.origin == me && e.inc == inc {
                        let (c0, c1) = (census(v0, me, inc, e.id), census(v1, me, inc, e.id));
                        assert(x(e));
                        assert(c1.b == c0.b.insert(seq, (false, c0.b[seq].1 + 1))) by {
                            assert(c0.b.contains_key(seq));
                        }
                        if relayed { assert(c1.r + 1 == c0.r && c1.p == c0.p); } else { assert(c1.p + 1 == c0.p && c1.r == c0.r); }
                        assert(c1.f == c0.f);
                        if !same(o0.at, if relayed { At::Relayed } else { At::Pending }) {
                            assert(v1.own[k].copied);
                            if let At::Batched { seq: s } = o0.at {
                                if s != seq { assert(c1.b[s] == c0.b[s]); }
                            }
                        } else if !o0.copied {
                            assert(c0.b.contains_key(seq) && !c0.b[seq].0);
                            assert(c0.b[seq].1 == 0);
                            assert forall|s: u64| #[trigger] c1.b.contains_key(s) && (!c1.b[s].0 || v1.own[k].at == (At::Batched { seq: s })) implies
                                c1.b[s].1 == if v1.own[k].at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                                if s != seq { assert(c0.b.contains_key(s)); }
                            }
                        }
                    } else {
                        assert(v1.own[k] == o0);
                        assert(census(v1, me, inc, o0.id) == census(v0, me, inc, o0.id));
                    }
                }
                assert forall|a: int| 0 <= a < taken@.len() implies !self.dedup@.contains(key(#[trigger] taken@[a])) by {
                    if a < bt.len() { assert(taken@[a] == bt[a]); }
                }
                assert forall|k| holds(prev.v(), k) implies holds(v1, k) || self.dedup@.contains(k) by {
                    if holds(v0, k) {
                        let mid = V { batches: v0.batches, ..v1 };
                        lemma_holds_pop(v0, mid, relayed, k);
                        let nb = v1.batches.len() - 1;
                        if k == key(e) {
                            assert(v1.batches[nb].entries[bt.len() as int] == e);
                        } else {
                            assert forall|b: int| 0 <= b < mid.batches.len() implies (#[trigger] v1.batches[b]).entries.len() >= mid.batches[b].entries.len()
                                && forall|i: int| 0 <= i < mid.batches[b].entries.len() ==> v1.batches[b].entries[i] == mid.batches[b].entries[i] by {
                                if b == nb {
                                    assert(v1.batches[b].entries == taken@ && mid.batches[b].entries == bt);
                                } else {
                                    assert(v1.batches[b] == mid.batches[b]);
                                }
                            }
                            lemma_holds_grow(mid, v1, k);
                        }
                    }
                }
                lemma_bsum_push(views(self.batches@), BatchV { seq, entries: taken@, slot: None });
                lemma_bsum_push(views(self.batches@), BatchV { seq, entries: bt, slot: None });
            }
        }
        if taken.len() == 0 {
            proof {
                let v1 = self.vcut(taken@);
                assert(taken@ =~= Seq::<Item>::empty());
                lemma_bsum_push(views(self.batches@), BatchV { seq, entries: taken@, slot: None });
                lemma_drop_empty_batch(self.v(), v1, me, inc);
                assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) || self.dedup@.contains(k) by {
                    if holds(v1, k) { lemma_holds_drop(v1, v1.batches.len() - 1, k); assert(v1.batches.remove(v1.batches.len() - 1) =~= self.v().batches); }
                }
                assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                    let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                    assert(prev.own@[k].id == id);
                }
                assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                    let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                    assert(self.own@[k].id == id);
                }
            }
            return None;
        }
        let ghost pre = *self;
        proof {
            lemma_bsum_push(views(self.batches@), BatchV { seq, entries: taken@, slot: None });
            lemma_count_le_any(taken@, every());
            assert(every() == |x: Item| true);
            assert(held(pre.vnew(taken@)) == held(pre.v()) + taken@.len());
        }
        let s = self.new_batch(taken);
        proof {
            assert(held(self.v()) <= held(prev.v()));
            let n = self.batches@.len() - 1;
            assert(self.batches@[n].entries@.len() > 0);
            assert(waits(self.batches@, s));
            assert(!self.dedup@.contains(key(self.batches@[n].entries@[0])));
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
            if !self.prune_due {
                assert forall|b: int| 0 <= b < self.batches@.len() && (#[trigger] self.batches@[b]).slot is None implies
                    exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])) by {
                    if b < n { assert(self.batches@[b] == prev.batches@[b]); } else { assert(!self.dedup@.contains(key(self.batches@[b].entries@[0]))); }
                }
            }
            assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {}
        }
        Some(s)
    }

    /// `fire_timers` drives a stalled slot with an empty batch: `new_batch(Vec::new())`, then
    /// `start`.
    pub fn takeover(&mut self, slot: u64)
        requires old(self).calm(), old(self).next_batch < u64::MAX,
        ensures final(self).calm(),
            final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            held(final(self).v()) == held(old(self).v()),
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let empty: Vec<Item> = Vec::new();
        proof {
            assert forall|k: int| 0 <= k < prev.own@.len() implies count(empty@, this(me, inc, (#[trigger] prev.own@[k]).id)) == 0 by {}
            lemma_e1_add_batch(prev.v(), me, inc, BatchV { seq: prev.next_batch, entries: empty@, slot: None });
        }
        let seq = self.new_batch(empty);
        proof {
            lemma_bsum_push(views(prev.batches@), BatchV { seq, entries: Seq::empty(), slot: None });
            let n = self.batches@.len() - 1;
            if !self.prune_due {
                assert forall|b: int| 0 <= b < self.batches@.len() && (#[trigger] self.batches@[b]).slot is None && self.batches@[b].seq != seq implies
                    exists|i: int| 0 <= i < self.batches@[b].entries@.len() && !self.dedup@.contains(key(#[trigger] self.batches@[b].entries@[i])) by {
                    assert(b < n);
                    assert(self.batches@[b] == prev.batches@[b]);
                }
            }
        }
        let ghost mid = *self;
        let started = self.start(seq, slot);
        proof {
            assert(started) by { assert(mid.batches@[mid.batches@.len() - 1].seq == seq); }
            assert forall|k| holds(prev.v(), k) implies holds(mid.v(), k) by {
                lemma_holds_prefix(prev.v(), mid.v(), k);
            }
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {}
            assert(held(mid.v()) == held(prev.v()));
        }
    }

    /// Batch `j` is out of its slot: the copies of own entries it keeps are no longer in a
    /// slot, so their entries are marked copied. DEV: marked before the slot is cleared (the
    /// two are independent); the caller sets `prune_due` first.
    #[verifier::rlimit(50)]
    fn unslot(&mut self, j: usize)
        requires old(self).inv(), old(self).prune_due, j < old(self).batches@.len(),
        ensures
            final(self).inv(), final(self).prune_due, final(self).same_ids(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).next_batch == old(self).next_batch, held(final(self).v()) == held(old(self).v()),
            old(self).settled() ==> final(self).settled(),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded,
            final(self).own@.len() == old(self).own@.len(),
            forall|k: int| 0 <= k < final(self).own@.len() ==> (#[trigger] final(self).own@[k]).id == old(self).own@[k].id,
            final(self).batches@.len() == old(self).batches@.len(),
            forall|b: int| 0 <= b < final(self).batches@.len() ==> (#[trigger] final(self).batches@[b]).seq == old(self).batches@[b].seq
                && final(self).batches@[b].entries@ == old(self).batches@[b].entries@
                && (b != j ==> final(self).batches@[b].slot == old(self).batches@[b].slot),
            final(self).batches@[j as int].slot is None,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let seq = self.batches[j].seq;
        let items = self.batches[j].entries.clone();
        let mut i = 0;
        while i < items.len()
            invariant
                i <= items@.len(), items@ == prev.batches@[j as int].entries@, j < self.batches@.len(),
                self.me == me, self.inc == inc, self.frame(prev), self.counted(), self.due(-1),
                self.pending == prev.pending, self.relayed == prev.relayed, self.forwarded == prev.forwarded,
                self.batches == prev.batches, self.unslotted == prev.unslotted,
                shape(self.v(), me, inc), e1(self.v(), me, inc),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id
                    && self.own@[k].at == prev.own@[k].at && self.own@[k].change == prev.own@[k].change
                    && (prev.own@[k].copied ==> self.own@[k].copied),
                forall|k: int, a: int| 0 <= k < self.own@.len() && 0 <= a < i && this(me, inc, (#[trigger] self.own@[k]).id)(#[trigger] items@[a]) ==>
                    self.own@[k].copied || self.own@[k].at == (At::Batched { seq }),
            decreases items.len() - i,
        {
            let e = items[i];
            if e.origin == self.me && e.inc == self.inc {
                if let Some(k) = self.find_own(e.id) {
                    let mut o = self.own[k];
                    if !same_exec(o.at, At::Batched { seq }) {
                        let ghost v0 = self.v();
                        o.copied = true;
                        self.set_own(k, o);
                        proof {
                            let v1 = self.v();
                            assert forall|a: int| 0 <= a < v0.own.len() && (#[trigger] v0.own[a]).id == e.id implies v1.own[a].id == e.id && fine(v1, me, inc, v1.own[a]) by {
                                if a != k { assert(v0.own[a].id != v0.own[k as int].id) by { if a < k {} else {} } }
                                assert(fine(v0, me, inc, v0.own[a]));
                            }
                            lemma_e1_step(v0, v1, me, inc, e.id);
                            assert(unique_own(v1.own)) by {
                                assert forall|a: int, b: int| 0 <= a < b < v1.own.len() implies v1.own[a].id != v1.own[b].id by {
                                    assert(v0.own[a].id != v0.own[b].id);
                                }
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        let ghost mid = *self;
        let nonempty = items.len() > 0;
        self.set_slot(j, None);
        if nonempty {
            self.unslotted_insert(seq);
        }
        proof {
            let (m, w) = (mid.v(), self.v());
            let nb = BatchV { seq, entries: items@, slot: None };
            assert(w.batches == m.batches.update(j as int, nb));
            lemma_bmap_update(m.batches, j as int, nb);
            lemma_bsum_update(m.batches, j as int, nb);
            assert(m.batches[j as int].seq == seq && m.batches[j as int].entries == items@);
            assert forall|k: int| 0 <= k < w.own.len() implies fine(w, me, inc, #[trigger] w.own[k]) by {
                let o = w.own[k];
                let x = this(me, inc, o.id);
                assert(fine(m, me, inc, o));
                lemma_bmap(m.batches, x);
                assert(bmap(m.batches, x).contains_key(m.batches[j as int].seq));
                let (c, d) = (census(m, me, inc, o.id), census(w, me, inc, o.id));
                assert(d.b == c.b.insert(seq, (false, count(items@, x))));
                if !o.copied && o.at != (At::Batched { seq }) {
                    assert forall|a: int| 0 <= a < items@.len() implies !x(#[trigger] items@[a]) by {
                        if x(items@[a]) {
                            assert(this(me, inc, w.own[k].id)(items@[a]));
                        }
                    }
                    lemma_count_none(items@, x);
                }
                if !o.copied {
                    assert forall|s: u64| #[trigger] d.b.contains_key(s) && (!d.b[s].0 || o.at == (At::Batched { seq: s })) implies
                        d.b[s].1 == if o.at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                        assert(c.b.contains_key(s));
                    }
                }
            }
            assert forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s) by {
                if waits(mid.batches@, s) && s != seq {
                    let b = choose|b: int| 0 <= b < mid.batches@.len() && (#[trigger] mid.batches@[b]).seq == s && mid.batches@[b].slot is None && mid.batches@[b].entries@.len() > 0;
                    assert(b != j);
                    assert(self.batches@[b] == mid.batches@[b]);
                }
                if s == seq && nonempty { assert(self.batches@[j as int].seq == s); }
                if waits(self.batches@, s) {
                    let b = choose|b: int| 0 <= b < self.batches@.len() && (#[trigger] self.batches@[b]).seq == s && self.batches@[b].slot is None && self.batches@[b].entries@.len() > 0;
                    if b != j {
                        assert(self.batches@[b] == mid.batches@[b]);
                        assert(waits(mid.batches@, s));
                    } else {
                        assert(nonempty);
                    }
                }
            }
            assert forall|k| holds(prev.v(), k) implies holds(w, k) by {
                lemma_holds_frame(m, w, k);
            }
            assert forall|b: int| 0 <= b < self.batches@.len() implies (#[trigger] self.batches@[b]).seq < self.next_batch by {
                if b != j { assert(self.batches@[b] == prev.batches@[b]); }
            }
            assert forall|id: u64| has_own(w, id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < w.own.len() && w.own[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(w, id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(w.own[k].id == id);
            }
            lemma_ocount_same(prev.own@, self.own@);
            if prev.settled() {
                assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                    assert(prev.own@[k].id == self.own@[k].id);
                }
            }
        }
    }

    /// Batch `seq` lost its slot and is proposed again in another one. DEV: `prune_due` set
    /// before the slot is cleared.
    pub fn lost_slot_kept(&mut self, seq: u64)
        requires old(self).calm(),
        ensures
            final(self).calm(), final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).next_batch == old(self).next_batch,
    {
        if let Some(j) = self.find_batch(seq) {
            self.prune_due = true;
            self.unslot(j);
        } else {
            proof { assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {} }
        }
    }

    /// Marks own entry `id` hedged.
    fn hedge_own(&mut self, id: u64)
        requires old(self).counted(),
        ensures
            final(self).counted(), final(self).frame(*old(self)),
            final(self).pending == old(self).pending, final(self).relayed == old(self).relayed,
            final(self).forwarded == old(self).forwarded, final(self).batches == old(self).batches,
            final(self).unslotted == old(self).unslotted, final(self).changes == old(self).changes,
            final(self).foreign == old(self).foreign, final(self).own@.len() == old(self).own@.len(),
            forall|k: int| 0 <= k < old(self).own@.len() ==> #[trigger] final(self).own@[k] == (Own { hedged: final(self).own@[k].hedged, ..old(self).own@[k] }),
    {
        if let Some(k) = self.find_own(id) {
            let mut o = self.own[k];
            o.hedged = true;
            self.set_own(k, o);
        }
    }

    /// The state `take_back` reasons about: with the entries of a batch taken out (`removed`)
    /// still to hand back counted as in a slot.
    pub open spec fn cur(self, removed: bool, seq: u64, rest: Seq<Item>) -> V {
        if removed { vback(self.v(), seq, rest) } else { self.v() }
    }

    /// The entries `items` of batch `seq`, handed back: own ones move out of it to
    /// `pending`; those of other members to `relayed` if `relay`, else to `pending`, unless
    /// `handed`; none in dedup. `removed`: the batch was taken out; else they are copies of a
    /// batch that stays in its slot. `hedge`: own ones are counted as hedged. DEV: flags for
    /// the callers' closures.
    #[verifier::rlimit(600)]
    fn take_back(&mut self, seq: u64, items: Vec<Item>, removed: bool, relay: bool, hedge: bool, handed: bool, dedup: &Dedup)
        requires
            old(self).counted(), shape(old(self).v(), old(self).me, old(self).inc),
            unresolved(old(self).own@, old(self).dedup@, old(self).me, old(self).inc),
            forall|k| dedup.has(k) <==> old(self).dedup@.contains(k),
            e1(old(self).cur(removed, seq, items@), old(self).me, old(self).inc),
            shape(old(self).cur(removed, seq, items@), old(self).me, old(self).inc),
            held(old(self).v()) + items@.len() <= 4 * held_max(),
            !removed ==> forall|j: int| 0 <= j < old(self).batches@.len() && (#[trigger] old(self).batches@[j]).seq == seq ==> old(self).batches@[j].slot is Some,
        ensures
            final(self).counted(), shape(final(self).v(), final(self).me, final(self).inc),
            e1(final(self).v(), final(self).me, final(self).inc),
            unresolved(final(self).own@, final(self).dedup@, final(self).me, final(self).inc),
            final(self).frame(*old(self)), final(self).batches == old(self).batches,
            final(self).forwarded == old(self).forwarded, final(self).unslotted == old(self).unslotted,
            final(self).own@.len() == old(self).own@.len(),
            forall|k: int| 0 <= k < final(self).own@.len() ==> (#[trigger] final(self).own@[k]).id == old(self).own@[k].id,
            // Handed back: appended to the queues, and only entries not in dedup.
            final(self).pending@.len() >= old(self).pending@.len(),
            final(self).pending@.subrange(0, old(self).pending@.len() as int) == old(self).pending@,
            forall|a: int| old(self).pending@.len() <= a < final(self).pending@.len() ==> !dedup.has(key(#[trigger] final(self).pending@[a])),
            final(self).relayed@.len() >= old(self).relayed@.len(),
            final(self).relayed@.subrange(0, old(self).relayed@.len() as int) == old(self).relayed@,
            forall|a: int| old(self).relayed@.len() <= a < final(self).relayed@.len() ==> !dedup.has(key(#[trigger] final(self).relayed@[a])),
            !relay ==> final(self).relayed == old(self).relayed,
            forall|k| holds(old(self).v(), k) ==> holds(final(self).v(), k),
            removed ==> forall|a: int| 0 <= a < items@.len() ==> holds(final(self).v(), key(#[trigger] items@[a])) || dedup.has(key(items@[a])) || handed,
            held(final(self).v()) <= held(old(self).v()) + items@.len(),
            // `handed`: no other member's entry is handed back (#187 D2).
            handed ==> final(self).relayed == old(self).relayed,
            handed ==> forall|a: int| old(self).pending@.len() <= a < final(self).pending@.len() ==>
                (#[trigger] final(self).pending@[a]).origin == old(self).me && final(self).pending@[a].inc == old(self).inc,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let mut i = 0;
        proof {
            assert(items@.subrange(0, items@.len() as int) =~= items@);
            if removed {
                let w = prev.cur(removed, seq, items@);
                assert forall|j: int| 0 <= j < prev.batches@.len() implies (#[trigger] views(prev.batches@)[j]).seq != seq by {
                    assert(w.batches[j] == views(prev.batches@)[j]);
                    assert(w.batches[prev.batches@.len() as int].seq == seq);
                }
            }
        }
        while i < items.len()
            invariant
                unique_own(prev.own@), unresolved(prev.own@, prev.dedup@, me, inc), prev.dedup == self.dedup,
                removed ==> forall|j: int| 0 <= j < prev.batches@.len() ==> (#[trigger] views(prev.batches@)[j]).seq != seq,
                !removed ==> forall|j: int| 0 <= j < prev.batches@.len() && (#[trigger] prev.batches@[j]).seq == seq ==> prev.batches@[j].slot is Some,
                i <= items@.len(), self.me == me, self.inc == inc, self.frame(prev), self.counted(),
                shape(self.v(), me, inc), unresolved(self.own@, self.dedup@, me, inc),
                forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                self.batches == prev.batches, self.forwarded == prev.forwarded, self.unslotted == prev.unslotted,
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                e1(self.cur(removed, seq, items@.subrange(i as int, items@.len() as int)), me, inc),
                shape(self.cur(removed, seq, items@.subrange(i as int, items@.len() as int)), me, inc),
                self.pending@.len() >= prev.pending@.len(),
                self.pending@.subrange(0, prev.pending@.len() as int) == prev.pending@,
                forall|a: int| prev.pending@.len() <= a < self.pending@.len() ==> !dedup.has(key(#[trigger] self.pending@[a])),
                self.relayed@.len() >= prev.relayed@.len(),
                self.relayed@.subrange(0, prev.relayed@.len() as int) == prev.relayed@,
                forall|a: int| prev.relayed@.len() <= a < self.relayed@.len() ==> !dedup.has(key(#[trigger] self.relayed@[a])),
                !relay ==> self.relayed == prev.relayed,
                forall|k| holds(prev.v(), k) ==> holds(self.v(), k),
                removed ==> forall|a: int| 0 <= a < i ==> holds(self.v(), key(#[trigger] items@[a])) || dedup.has(key(items@[a])) || handed,
                held(self.v()) <= held(prev.v()) + i, held(prev.v()) + items@.len() <= 4 * held_max(),
                handed ==> self.relayed == prev.relayed,
                handed ==> forall|a: int| prev.pending@.len() <= a < self.pending@.len() ==>
                    (#[trigger] self.pending@[a]).origin == me && self.pending@[a].inc == inc,
            decreases items.len() - i,
        {
            let x = items[i];
            let ghost before = *self;
            let ghost rest0 = items@.subrange(i as int, items@.len() as int);
            let ghost rest1 = items@.subrange(i + 1, items@.len() as int);
            let ghost v0 = before.cur(removed, seq, rest0);
            proof {
                lemma_count_pop(rest0);
                assert(rest0.subrange(1, rest0.len() as int) =~= rest1);
                assert(rest0[0] == x);
                if removed {
                    lemma_bmap_push(views(self.batches@), BatchV { seq, entries: rest0, slot: Some(0) });
                    lemma_bmap_push(views(self.batches@), BatchV { seq, entries: rest1, slot: Some(0) });
                }
            }
            if dedup.contains(x.origin, x.inc, x.id) {
                proof {
                    let v1 = self.cur(removed, seq, rest1);
                    assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                        let o = v0.own[k];
                        assert(fine(v0, me, inc, o));
                        assert(!self.dedup@.contains((me, inc, o.id)));
                        assert(!this(me, inc, o.id)(x));
                        assert(census(v1, me, inc, o.id) == census(v0, me, inc, o.id));
                    }
                }
                i += 1;
                continue;
            }
            if handed && !(x.origin == self.me && x.inc == self.inc) {
                proof {
                    let v1 = self.cur(removed, seq, rest1);
                    assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                        let o = v0.own[k];
                        assert(fine(v0, me, inc, o));
                        assert(!this(me, inc, o.id)(x));
                        assert(census(v1, me, inc, o.id) == census(v0, me, inc, o.id));
                    }
                }
                i += 1;
                continue;
            }
            if hedge && x.origin == self.me {
                let ghost h0 = *self;
                self.hedge_own(x.id);
                proof {
                    lemma_e1_own_fields(h0.cur(removed, seq, rest0), self.cur(removed, seq, rest0), me, inc);
                    lemma_ocount_same(h0.own@, self.own@);
                }
            }
            let ghost mv0 = *self;
            if x.origin == self.me && x.inc == self.inc {
                self.moved(x.id, At::Batched { seq }, At::Pending);
            }
            proof { self.lemma_bounded(); }
            let ghost pushed = *self;
            if x.origin == self.me || !relay {
                self.push_pending(x);
            } else {
                self.push_relayed(x);
            }
            proof {
                let (a0, v1) = (mv0.cur(removed, seq, rest0), self.cur(removed, seq, rest1));
                lemma_count_push(pushed.pending@, x);
                lemma_count_push(pushed.relayed@, x);
                assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                    let o0 = a0.own[k];
                    assert(fine(a0, me, inc, o0));
                    let (c0, c1) = (census(a0, me, inc, o0.id), census(v1, me, inc, o0.id));
                    if x.origin == me && x.inc == inc && o0.id == x.id {
                        assert(this(me, inc, o0.id)(x));
                        assert(c1.p == c0.p + 1 && c1.r == c0.r && c1.f == c0.f);
                        if removed {
                            assert(c0.b.contains_key(seq) && c0.b[seq].0);
                            assert(c1.b == c0.b.insert(seq, (true, (c0.b[seq].1 - 1) as nat)));
                        } else {
                            assert(c1.b == c0.b);
                        }
                        if !same(o0.at, At::Batched { seq }) {
                            assert(v1.own[k].copied);
                            if let At::Batched { seq: s } = o0.at { assert(s != seq); assert(c1.b[s] == c0.b[s]); }
                        } else if !o0.copied {
                            if !removed {
                                lemma_bmap(views(self.batches@), this(me, inc, o0.id));
                                assert(c0.b.contains_key(seq));
                                let jj = choose|jj: int| 0 <= jj < views(self.batches@).len() && views(self.batches@)[jj].seq == seq;
                                assert(views(self.batches@)[jj] == bv(prev.batches@[jj]));
                                assert(prev.batches@[jj].slot is Some);
                                assert(c0.b[seq].0);
                            }
                            assert forall|s: u64| #[trigger] c1.b.contains_key(s) && (!c1.b[s].0 || v1.own[k].at == (At::Batched { seq: s })) implies
                                c1.b[s].1 == if v1.own[k].at == (At::Batched { seq: s }) { 1nat } else { 0nat } by {
                                if s != seq { assert(c0.b.contains_key(s)); }
                            }
                        }
                    } else {
                        assert(v1.own[k] == o0);
                        assert(!this(me, inc, o0.id)(x));
                        assert(c1 == c0);
                    }
                }
                assert forall|k: int| 0 <= k < self.own@.len() implies (#[trigger] self.own@[k]).id == prev.own@[k].id by {
                    assert(before.own@[k].id == prev.own@[k].id);
                    assert(mv0.own@[k] == (Own { hedged: mv0.own@[k].hedged, ..before.own@[k] }) || mv0.own@[k] == before.own@[k]);
                    assert(pushed.own@[k].id == mv0.own@[k].id);
                }
                assert(unique_own(v1.own)) by {
                    assert forall|a: int, b: int| 0 <= a < b < v1.own.len() implies v1.own[a].id != v1.own[b].id by {
                        assert(prev.own@[a].id != prev.own@[b].id);
                    }
                }
                assert(self.pending@.subrange(0, before.pending@.len() as int) =~= before.pending@);
                assert(self.relayed@.subrange(0, before.relayed@.len() as int) =~= before.relayed@);
                assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                    assert(before.own@[k].id == prev.own@[k].id);
                    assert(mv0.own@[k] == (Own { hedged: mv0.own@[k].hedged, ..before.own@[k] }) || mv0.own@[k] == before.own@[k]);
                    assert(pushed.own@[k].id == mv0.own@[k].id);
                    assert(prev.own@[k].id == self.own@[k].id);
                }
                assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) by {
                    lemma_holds_extend(before.v(), self.v(), k);
                }
                if handed {
                    assert(x.origin == me && x.inc == inc);
                    assert forall|a: int| prev.pending@.len() <= a < self.pending@.len() implies
                        (#[trigger] self.pending@[a]).origin == me && self.pending@[a].inc == inc by {
                        if a < before.pending@.len() { assert(self.pending@[a] == before.pending@[a]); }
                    }
                }
                if removed {
                    assert forall|a: int| 0 <= a < i + 1 implies holds(self.v(), key(#[trigger] items@[a])) || dedup.has(key(items@[a])) || handed by {
                        if a < i { if holds(before.v(), key(items@[a])) { lemma_holds_extend(before.v(), self.v(), key(items@[a])); } }
                        else if x.origin == me || !relay { assert(key(self.pending@[self.pending@.len() - 1]) == key(items@[a])); }
                        else { assert(key(self.relayed@[self.relayed@.len() - 1]) == key(items@[a])); }
                    }
                }
                assert(self.pending@.subrange(0, prev.pending@.len() as int) =~= prev.pending@);
                assert(self.relayed@.subrange(0, prev.relayed@.len() as int) =~= prev.relayed@);
            }
            i += 1;
        }
        proof {
            if removed {
                let rest = items@.subrange(i as int, items@.len() as int);
                assert(rest =~= Seq::<Item>::empty());
                lemma_drop_empty_batch(self.v(), self.cur(removed, seq, rest), me, inc);
            }
        }
    }

    /// Batch `seq` lost its slot and its entries are handed back: own ones (and, if the slot
    /// went to the target `deciding`, every one) to `pending`, those of other members to
    /// `relayed`, unless the batch was handed on when stepping aside.
    #[verifier::rlimit(200)]
    pub fn lost_slot(&mut self, seq: u64, deciding: bool, dedup: &Dedup)
        requires old(self).ready(*dedup),
        ensures
            final(self).ready(*dedup), final(self).same_ids(*old(self)),
            forall|id: u64| has_own(old(self).v(), id) ==> has_own(final(self).v(), id),
            // E3, but for the entries of a batch handed on when stepping aside: they went to
            // the target then.
            forall|k: (u64, u64, u64)| holds(old(self).v(), k) ==> holds(final(self).v(), k) || final(self).dedup@.contains(k)
                || (handed_on(old(self).batches@, seq) && in_batch(old(self).v(), seq, k)),
            final(self).next_batch == old(self).next_batch,
            final(self).forwarded == old(self).forwarded,
            // A batch handed on hands back no other member's entry (#187 D2).
            handed_on(old(self).batches@, seq) ==> final(self).relayed == old(self).relayed,
            handed_on(old(self).batches@, seq) ==> forall|a: int| old(self).pending@.len() <= a < final(self).pending@.len() ==>
                (#[trigger] final(self).pending@[a]).origin == old(self).me && final(self).pending@[a].inc == old(self).inc,
            // A lost slot hands back only entries not in dedup.
            final(self).pending@.subrange(0, old(self).pending@.len() as int) == old(self).pending@,
            forall|a: int| old(self).pending@.len() <= a < final(self).pending@.len() ==> !dedup.has(key(#[trigger] final(self).pending@[a])),
            final(self).relayed@.subrange(0, old(self).relayed@.len() as int) == old(self).relayed@,
            forall|a: int| old(self).relayed@.len() <= a < final(self).relayed@.len() ==> !dedup.has(key(#[trigger] final(self).relayed@[a])),
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let j = match self.find_batch(seq) {
            Some(j) => j,
            None => {
                proof {
                    assert(self.pending@.subrange(0, self.pending@.len() as int) =~= self.pending@);
                    assert(self.relayed@.subrange(0, self.relayed@.len() as int) =~= self.relayed@);
                    assert forall|k| holds(self.v(), k) implies holds(self.v(), k) by {}
                    assert(!handed_on(self.batches@, seq));
                }
                return;
            },
        };
        proof { lemma_e1_back(prev.v(), me, inc, j as int); }
        let b = self.remove_batch(j);
        let ghost mid = *self;
        proof {
            assert(self.v() == V { batches: prev.v().batches.remove(j as int), ..prev.v() });
            assert(prev.v().batches[j as int] == bv(b));
            lemma_bsum_remove(views(prev.batches@), j as int);
            lemma_count_le_any(b.entries@, every());
            assert(every() == |x: Item| true);
            assert(held(mid.v()) + b.entries@.len() == held(prev.v()));
        }
        let handed = b.handed;
        proof {
            assert(handed_on(prev.batches@, seq) == handed) by {
                if handed_on(prev.batches@, seq) {
                    let jj = choose|jj: int| 0 <= jj < prev.batches@.len() && (#[trigger] prev.batches@[jj]).seq == seq && prev.batches@[jj].handed;
                    assert(prev.v().batches[jj].seq == prev.v().batches[j as int].seq);
                    lemma_one_seq(prev.v(), me, inc, jj, j as int);
                }
            }
        }
        self.take_back(seq, b.entries, true, !deciding, false, handed, dedup);
        proof {
            self.lemma_due_remove(prev, j as int);
            assert forall|s: int| 0 <= s < self.batches@.len() implies (#[trigger] self.batches@[s]).seq < self.next_batch by {
                let s2 = if s < j { s } else { s + 1 };
                assert(self.batches@[s] == prev.batches@[s2]);
            }
            assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) || self.dedup@.contains(k)
                || (handed_on(prev.batches@, seq) && in_batch(prev.v(), seq, k)) by {
                lemma_holds_drop(prev.v(), j as int, k);
                if !holds(mid.v(), k) {
                    let a = choose|a: int| 0 <= a < prev.v().batches[j as int].entries.len() && key(#[trigger] prev.v().batches[j as int].entries[a]) == k;
                    assert(b.entries@[a] == prev.v().batches[j as int].entries[a]);
                    assert(prev.v().batches[j as int].seq == seq);
                }
            }
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
        }
    }

    /// Batch `j`, not in a slot, taken out and handed back to `pending`; the rest of the
    /// invariant is kept. Shared by `lost_lead` and `step_aside`.
    #[verifier::rlimit(200)]
    fn hand_back_unstarted(&mut self, j: usize, hedge: bool, dedup: &Dedup)
        requires
            old(self).inv(), unresolved(old(self).own@, old(self).dedup@, old(self).me, old(self).inc),
            forall|k| dedup.has(k) <==> old(self).dedup@.contains(k),
            j < old(self).batches@.len(), old(self).batches@[j as int].slot is None,
            held(old(self).v()) <= 3 * held_max(),
        ensures
            final(self).inv(), unresolved(final(self).own@, final(self).dedup@, final(self).me, final(self).inc),
            final(self).frame(*old(self)), final(self).forwarded == old(self).forwarded,
            final(self).relayed == old(self).relayed,
            held(final(self).v()) <= held(old(self).v()),
            final(self).own@.len() == old(self).own@.len(),
            forall|k: int| 0 <= k < final(self).own@.len() ==> (#[trigger] final(self).own@[k]).id == old(self).own@[k].id,
            forall|k| holds(old(self).v(), k) ==> holds(final(self).v(), k) || final(self).dedup@.contains(k),
            final(self).batches@ == old(self).batches@.remove(j as int),
            views(final(self).batches@) == views(old(self).batches@).remove(j as int),
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let seq = self.batches[j].seq;
        proof { lemma_e1_back(prev.v(), me, inc, j as int); }
        let b = self.remove_batch(j);
        let ghost mid = *self;
        proof {
            assert(self.v() == V { batches: prev.v().batches.remove(j as int), ..prev.v() });
            assert(prev.v().batches[j as int] == bv(b));
            lemma_bsum_remove(views(prev.batches@), j as int);
            lemma_count_le_any(b.entries@, every());
            assert(every() == |x: Item| true);
            assert(held(mid.v()) + b.entries@.len() == held(prev.v()));
        }
        self.take_back(seq, b.entries, true, false, hedge, false, dedup);
        proof {
            self.lemma_due_remove(prev, j as int);
            assert forall|s: int| 0 <= s < self.batches@.len() implies (#[trigger] self.batches@[s]).seq < self.next_batch by {
                let s2 = if s < j { s } else { s + 1 };
                assert(self.batches@[s] == prev.batches@[s2]);
            }
            assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) || self.dedup@.contains(k) by {
                lemma_holds_drop(prev.v(), j as int, k);
                if !holds(mid.v(), k) {
                    let a = choose|a: int| 0 <= a < prev.v().batches[j as int].entries.len() && key(#[trigger] prev.v().batches[j as int].entries[a]) == k;
                    assert(b.entries@[a] == prev.v().batches[j as int].entries[a]);
                }
            }
        }
    }

    /// No longer the leader: the entries of batches not yet in a slot go to `pending`.
    /// DEV: one batch at a time, in seq order (the Rust code takes them all out first; no
    /// step reads the others).
    #[verifier::rlimit(200)]
    pub fn lost_lead(&mut self, dedup: &Dedup)
        requires old(self).ready(*dedup),
        ensures
            final(self).ready(*dedup), final(self).same_ids(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()), final(self).next_batch == old(self).next_batch,
            final(self).forwarded == old(self).forwarded, final(self).relayed == old(self).relayed,
            // Losing the lead keeps every batch in a slot, and only those.
            forall|b: int| 0 <= b < old(self).batches@.len() && (#[trigger] old(self).batches@[b]).slot is Some ==>
                exists|c: int| 0 <= c < final(self).batches@.len() && bv(#[trigger] final(self).batches@[c]) == bv(old(self).batches@[b]),
            forall|c: int| 0 <= c < final(self).batches@.len() ==> (#[trigger] final(self).batches@[c]).slot is Some,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let mut j = 0;
        while j < self.batches.len()
            invariant
                j <= self.batches@.len(), self.me == me, self.inc == inc, self.dedup == prev.dedup,
                self.next_batch == prev.next_batch, self.prune_due == prev.prune_due,
                self.inv(), self.forwarded == prev.forwarded, self.relayed == prev.relayed,
                unresolved(self.own@, self.dedup@, me, inc),
                forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                held(self.v()) <= held(prev.v()), held(prev.v()) <= held_max(),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                forall|k| holds(prev.v(), k) ==> holds(self.v(), k) || self.dedup@.contains(k),
                forall|b: int| 0 <= b < prev.batches@.len() && (#[trigger] prev.batches@[b]).slot is Some ==>
                    exists|c: int| 0 <= c < self.batches@.len() && bv(#[trigger] self.batches@[c]) == bv(prev.batches@[b]),
                forall|c: int| 0 <= c < j ==> (#[trigger] self.batches@[c]).slot is Some,
            decreases self.batches.len() - j,
        {
            if self.batches[j].slot.is_none() {
                let ghost before = *self;
                self.hand_back_unstarted(j, false, dedup);
                proof {
                    assert forall|b: int| 0 <= b < prev.batches@.len() && (#[trigger] prev.batches@[b]).slot is Some implies
                        exists|c: int| 0 <= c < self.batches@.len() && bv(#[trigger] self.batches@[c]) == bv(prev.batches@[b]) by {
                        let c = choose|c: int| 0 <= c < before.batches@.len() && bv(#[trigger] before.batches@[c]) == bv(prev.batches@[b]);
                        assert(c != j);
                        let c2 = if c < j { c } else { c - 1 };
                        assert(self.batches@[c2] == before.batches@[c]);
                    }
                    assert forall|c: int| 0 <= c < j implies (#[trigger] self.batches@[c]).slot is Some by {
                        assert(self.batches@[c] == before.batches@[c]);
                    }
                    assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) || self.dedup@.contains(k) by {
                        if holds(before.v(), k) {}
                    }
                }
            } else {
                j += 1;
            }
        }
        proof {
            assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {}
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
        }
    }

    /// Stepped aside: the entries of every local batch go to `pending` (a batch already in a
    /// slot stays there too), own ones counted as hedged, and so does everything relayed.
    /// DEV: one batch at a time, in seq order. Copies may double what is held, so the bound
    /// on what is held at a call is the caller's again.
    #[verifier::rlimit(400)]
    pub fn step_aside(&mut self, dedup: &Dedup)
        requires old(self).ready(*dedup),
        ensures
            final(self).inv(), final(self).settled(), forall|k| dedup.has(k) <==> final(self).dedup@.contains(k),
            held(final(self).v()) <= 2 * held_max(),
            final(self).same_ids(*old(self)),
            final(self).kept(*old(self), Set::empty(), Seq::empty()), final(self).next_batch == old(self).next_batch,
            final(self).forwarded == old(self).forwarded,
            // Stepping aside leaves nothing relayed, no batch waiting for a slot, and every
            // batch marked handed on.
            final(self).relayed@.len() == 0,
            forall|c: int| 0 <= c < final(self).batches@.len() ==> (#[trigger] final(self).batches@[c]).slot is Some,
            forall|c: int| 0 <= c < final(self).batches@.len() ==> (#[trigger] final(self).batches@[c]).handed,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let mut j = 0;
        proof {
            assert(views(self.batches@).subrange(0, 0) =~= Seq::<BatchV>::empty());
            assert(bsum(Seq::<BatchV>::empty(), every()) == 0);
            lemma_bsum_le(views(self.batches@), every());
        }
        while j < self.batches.len()
            invariant
                j <= self.batches@.len(), self.me == me, self.inc == inc, self.dedup == prev.dedup,
                self.next_batch == prev.next_batch, self.prune_due == prev.prune_due,
                self.inv(), self.forwarded == prev.forwarded,
                unresolved(self.own@, self.dedup@, me, inc),
                forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                held(self.v()) <= held(prev.v()) + bsum(views(self.batches@).subrange(0, j as int), every()),
                bsum(views(self.batches@), every()) <= bsum(views(prev.batches@), every()),
                bsum(views(prev.batches@), every()) <= held(prev.v()), held(prev.v()) <= held_max(),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                forall|k| holds(prev.v(), k) ==> holds(self.v(), k) || self.dedup@.contains(k),
                forall|c: int| 0 <= c < j ==> (#[trigger] self.batches@[c]).slot is Some,
                forall|c: int| 0 <= c < j ==> (#[trigger] self.batches@[c]).handed,
            decreases self.batches.len() - j,
        {
            proof {
                lemma_bsum_prefix(views(self.batches@), j as int);
                lemma_bsum_prefix(views(self.batches@), j + 1);
            }
            let ghost before = *self;
            if self.batches[j].slot.is_none() {
                proof {
                    lemma_bsum_remove(views(before.batches@), j as int);
                    assert(views(before.batches@).remove(j as int).subrange(0, j as int) =~= views(before.batches@).subrange(0, j as int));
                }
                self.hand_back_unstarted(j, true, dedup);
                proof {
                    assert forall|c: int| 0 <= c < j implies (#[trigger] self.batches@[c]).slot is Some by {
                        assert(self.batches@[c] == before.batches@[c]);
                    }
                    assert forall|c: int| 0 <= c < j implies (#[trigger] self.batches@[c]).handed by {
                        assert(self.batches@[c] == before.batches@[c]);
                    }
                    assert forall|k| holds(prev.v(), k) implies holds(self.v(), k) || self.dedup@.contains(k) by {
                        if holds(before.v(), k) {}
                    }
                }
            } else {
                let seq = self.batches[j].seq;
                let items = self.batches[j].entries.clone();
                proof {
                    let vb = views(before.batches@);
                    assert(vb[j as int] == bv(before.batches@[j as int]));
                    assert(vb.subrange(0, j + 1) =~= vb.subrange(0, j as int).push(vb[j as int]));
                    lemma_bsum_push(vb.subrange(0, j as int), vb[j as int]);
                    lemma_count_le_any(items@, every());
                    assert(every() == |x: Item| true);
                    assert forall|jj: int| 0 <= jj < before.batches@.len() && (#[trigger] before.batches@[jj]).seq == seq implies before.batches@[jj].slot is Some by {
                        assert(vb[jj].seq == vb[j as int].seq);
                        lemma_one_seq(before.v(), me, inc, jj, j as int);
                    }
                    assert(before.cur(false, seq, items@) == before.v());
                }
                self.take_back(seq, items, false, false, true, false, dedup);
                self.set_handed(j);
                proof {
                    assert forall|c: int| 0 <= c < j + 1 implies (#[trigger] self.batches@[c]).slot is Some by {
                        if c != j { assert(self.batches@[c] == before.batches@[c]); }
                    }
                    assert forall|c: int| 0 <= c < j + 1 implies (#[trigger] self.batches@[c]).handed by {
                        if c != j { assert(self.batches@[c] == before.batches@[c]); }
                    }
                }
                j += 1;
            }
        }
        let ghost mid = *self;
        proof {
            lemma_bsum_prefix(views(self.batches@), j as int);
            assert(views(self.batches@).subrange(0, j as int) =~= views(self.batches@));
        }
        while self.relayed.len() > 0
            invariant
                self.me == me, self.inc == inc, self.dedup == prev.dedup, self.inv(),
                self.next_batch == prev.next_batch, self.forwarded == prev.forwarded, self.batches == mid.batches,
                unresolved(self.own@, self.dedup@, me, inc),
                forall|k| dedup.has(k) <==> self.dedup@.contains(k),
                held(self.v()) == held(mid.v()), held(mid.v()) <= 2 * held_max(),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                forall|k| holds(prev.v(), k) ==> holds(self.v(), k) || self.dedup@.contains(k),
                forall|c: int| 0 <= c < self.batches@.len() ==> (#[trigger] self.batches@[c]).slot is Some,
                forall|c: int| 0 <= c < self.batches@.len() ==> (#[trigger] self.batches@[c]).handed,
            decreases self.relayed@.len(),
        {
            let ghost before = *self;
            proof { self.lemma_bounded(); }
            let e = match self.pop(true) {
                Some(e) => e,
                None => {
                    proof { assert(false); }
                    return;
                },
            };
            let ghost popped = *self;
            if e.origin == self.me && e.inc == self.inc {
                self.moved(e.id, At::Relayed, At::Pending);
            }
            let ghost mv = *self;
            proof {
                self.lemma_bounded();
                assert(held(self.v()) < held(before.v()));
            }
            self.push_pending(e);
            proof {
                let (v0, v1) = (before.v(), self.v());
                lemma_count_pop(before.relayed@);
                lemma_count_push(mv.pending@, e);
                assert(v1.relayed == v0.relayed.subrange(1, v0.relayed.len() as int));
                assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                    let o0 = v0.own[k];
                    assert(fine(v0, me, inc, o0));
                    let (c0, c1) = (census(v0, me, inc, o0.id), census(v1, me, inc, o0.id));
                    if e.origin == me && e.inc == inc && o0.id == e.id {
                        assert(this(me, inc, o0.id)(e));
                        assert(c1.p == c0.p + 1 && c1.r + 1 == c0.r && c1.f == c0.f && c1.b == c0.b);
                    } else {
                        assert(v1.own[k] == o0);
                        assert(!this(me, inc, o0.id)(e));
                        assert(c1 == c0);
                    }
                }
                assert(unique_own(v1.own)) by {
                    assert forall|a: int, b: int| 0 <= a < b < v1.own.len() implies v1.own[a].id != v1.own[b].id by {
                        assert(v0.own[a].id != v0.own[b].id);
                    }
                }
                lemma_ocount_same(v0.own, v1.own);
                assert forall|k: int| 0 <= k < self.own@.len() implies (#[trigger] self.own@[k]).id == prev.own@[k].id by {
                    assert(before.own@[k].id == prev.own@[k].id);
                }
                assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                    assert(before.own@[k].id == self.own@[k].id);
                }
                assert forall|k| holds(prev.v(), k) implies holds(v1, k) || self.dedup@.contains(k) by {
                    if holds(v0, k) {
                        lemma_holds_pop(v0, V { pending: v0.pending, ..popped.v() }, true, k);
                        assert(v1.pending.subrange(0, popped.pending@.len() as int) =~= popped.pending@);
                        assert(v1.relayed.subrange(0, popped.relayed@.len() as int) =~= popped.relayed@);
                        if k == key(e) {
                            assert(key(v1.pending[v1.pending.len() - 1]) == k);
                        } else {
                            lemma_holds_extend(V { pending: v0.pending, ..popped.v() }, v1, k);
                        }
                    }
                }
            }
        }
        proof {
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
        }
    }

    /// The target is suspected: everything handed to it goes back to the front of `pending`,
    /// counted as hedged. Returns how many. DEV: popped from the back for `into_iter().rev()`.
    #[verifier::rlimit(300)]
    pub fn suspect(&mut self) -> (n: u64)
        requires old(self).calm(),
        ensures
            final(self).calm(), final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).next_batch == old(self).next_batch, final(self).forwarded@.len() == 0,
            n == old(self).forwarded@.len(),
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let mut f: Vec<(Item, u64)> = Vec::new();
        std::mem::swap(&mut self.forwarded, &mut f);
        let mut n: u64 = 0;
        proof { assert(V { fwd: f@, ..self.v() } == prev.v()); }
        while f.len() > 0
            invariant
                self.me == me, self.inc == inc, self.frame(prev), self.counted(),
                self.forwarded@.len() == 0, self.batches == prev.batches, self.unslotted == prev.unslotted,
                self.relayed == prev.relayed, self.foreign == prev.foreign,
                shape(V { fwd: f@, ..self.v() }, me, inc), e1(V { fwd: f@, ..self.v() }, me, inc),
                unresolved(self.own@, self.dedup@, me, inc),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                held(V { fwd: f@, ..self.v() }) == held(prev.v()), held(prev.v()) <= held_max(),
                n + f@.len() == prev.forwarded@.len(),
                forall|k| holds(prev.v(), k) ==> holds(V { fwd: f@, ..self.v() }, k),
            decreases f@.len(),
        {
            let ghost before = *self;
            let ghost fb = f@;
            let ghost v0 = V { fwd: fb, ..before.v() };
            let (e, since) = match f.pop() {
                Some(p) => p,
                None => {
                    proof { assert(false); }
                    return n;
                },
            };
            proof {
                assert(f@ =~= fb.drop_last());
                assert(fb.remove(fb.len() - 1) =~= f@);
                lemma_fmap_remove(fb, fb.len() - 1);
                lemma_fmap(fb);
                assert(fb[fb.len() - 1] == (e, since));
                assert(e.origin == me && e.inc == inc);
            }
            if let Some(k) = self.find_own(e.id) {
                let mut o = self.own[k];
                o.hedged = true;
                if let At::Forwarded { .. } = o.at {
                    o.at = At::Pending;
                }
                self.set_own(k, o);
            }
            n = n + 1;
            let ghost mv = *self;
            proof {
                self.lemma_bounded();
                assert(held(self.v()) < held(v0));
            }
            if e.change {
                self.changes = self.changes + 1;
            }
            self.pending.insert(0, e);
            proof {
                let v1 = V { fwd: f@, ..self.v() };
                lemma_count_front(mv.pending@, e);
                assert(v1.pending == v0.pending.insert(0, e));
                assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                    let o0 = v0.own[k];
                    assert(fine(v0, me, inc, o0));
                    let (c0, c1) = (census(v0, me, inc, o0.id), census(v1, me, inc, o0.id));
                    if o0.id == e.id {
                        assert(this(me, inc, o0.id)(e));
                        assert(fmap(v0.fwd).contains_key(e.id));
                        assert(c0.f == Some(since));
                        assert(c1.p == c0.p + 1 && c1.r == c0.r && c1.f is None && c1.b == c0.b);
                    } else {
                        assert(v1.own[k] == o0);
                        assert(!this(me, inc, o0.id)(e));
                        assert(c1 == c0);
                    }
                }
                assert(unique_own(v1.own)) by {
                    assert forall|a: int, b: int| 0 <= a < b < v1.own.len() implies v1.own[a].id != v1.own[b].id by {
                        assert(v0.own[a].id != v0.own[b].id);
                    }
                }
                assert(unique_ids(f@)) by {
                    assert forall|a: int, b: int| 0 <= a < b < f@.len() implies f@[a].0.id != f@[b].0.id by {
                        assert(fb[a] == f@[a] && fb[b] == f@[b]);
                    }
                }
                assert forall|a: int| 0 <= a < f@.len() implies (#[trigger] f@[a]).0.origin == me && f@[a].0.inc == inc by {
                    assert(fb[a] == f@[a]);
                }
                lemma_ocount_same(v0.own, v1.own);
                assert forall|k: int| 0 <= k < self.own@.len() implies (#[trigger] self.own@[k]).id == prev.own@[k].id by {
                    assert(before.own@[k].id == prev.own@[k].id);
                }
                assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                    assert(before.own@[k].id == self.own@[k].id);
                }
                assert forall|k| holds(prev.v(), k) implies holds(v1, k) by {
                    if holds(v0, k) {
                        lemma_holds_fwd_remove(v0, fb.len() - 1, k);
                        if k == key(e) {
                            assert(key(v1.pending[0]) == k);
                        } else {
                            lemma_holds_frame(V { fwd: f@, ..v0 }, V { fwd: f@, ..mv.v() }, k);
                            lemma_holds_front(V { fwd: f@, ..mv.v() }, v1, e, k);
                        }
                    }
                }
            }
        }
        proof {
            assert(f@ =~= Seq::<(Item, u64)>::empty());
            assert(self.forwarded@ =~= f@);
            assert(V { fwd: f@, ..self.v() } == self.v());
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
        }
        n
    }

    /// The target left the entries `late` uncommitted too long: proposed here (counted as
    /// hedged), or, `aside`, handed to the target again. Returns how many were hedged.
    #[verifier::rlimit(300)]
    pub fn late(&mut self, late: Vec<u64>, aside: bool) -> (hedged: u64)
        requires old(self).calm(), late@.len() <= held_max(),
        ensures
            final(self).calm(), final(self).same_ids(*old(self)), final(self).kept(*old(self), Set::empty(), Seq::empty()),
            final(self).next_batch == old(self).next_batch, aside ==> hedged == 0,
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        let mut hedged: u64 = 0;
        let mut i = 0;
        while i < late.len()
            invariant
                i <= late@.len(), late@.len() <= held_max(), hedged <= i, aside ==> hedged == 0,
                self.me == me, self.inc == inc, self.frame(prev), self.inv(),
                self.batches == prev.batches, self.unslotted == prev.unslotted,
                unresolved(self.own@, self.dedup@, me, inc),
                self.own@.len() == prev.own@.len(),
                forall|k: int| 0 <= k < self.own@.len() ==> (#[trigger] self.own@[k]).id == prev.own@[k].id,
                held(self.v()) == held(prev.v()), held(prev.v()) <= held_max(),
                forall|k| holds(prev.v(), k) ==> holds(self.v(), k),
            decreases late.len() - i,
        {
            let id = late[i];
            if let Some(j) = self.find_fwd(id) {
                let ghost before = *self;
                let ghost v0 = before.v();
                proof { lemma_fmap_remove(self.forwarded@, j as int); lemma_fmap(self.forwarded@); }
                let (e, since) = self.forwarded.remove(j);
                let ghost removed = *self;
                proof {
                    assert(removed.forwarded@ == before.forwarded@.remove(j as int));
                    assert(e.origin == me && e.inc == inc && e.id == id);
                    assert forall|a: int| 0 <= a < self.forwarded@.len() implies (#[trigger] self.forwarded@[a]).0.origin == me && self.forwarded@[a].0.inc == inc by {
                        let a2 = if a < j { a } else { a + 1 };
                        assert(self.forwarded@[a] == before.forwarded@[a2]);
                    }
                }
                let from = At::Forwarded { since };
                if aside {
                    self.moved(id, from, At::Pending);
                    proof { self.lemma_bounded(); }
                    self.push_pending(e);
                } else {
                    self.moved(id, from, At::Relayed);
                    let ghost h0 = *self;
                    self.hedge_own(id);
                    proof {
                        lemma_ocount_same(h0.own@, self.own@);
                        self.lemma_bounded();
                    }
                    hedged = hedged + 1;
                    self.push_relayed(e);
                }
                proof {
                    let v1 = self.v();
                    lemma_count_push(removed.pending@, e);
                    lemma_count_push(removed.relayed@, e);
                    assert(e.origin == me);
                    assert forall|k: int| 0 <= k < v1.own.len() implies fine(v1, me, inc, #[trigger] v1.own[k]) by {
                        let o0 = v0.own[k];
                        assert(fine(v0, me, inc, o0));
                        let (c0, c1) = (census(v0, me, inc, o0.id), census(v1, me, inc, o0.id));
                        if o0.id == id {
                            assert(this(me, inc, o0.id)(e));
                            assert(c0.f == Some(since));
                            if aside {
                                assert(c1.p == c0.p + 1 && c1.r == c0.r && c1.f is None && c1.b == c0.b);
                            } else {
                                assert(c1.r == c0.r + 1 && c1.p == c0.p && c1.f is None && c1.b == c0.b);
                            }
                        } else {
                            assert(v1.own[k].id == o0.id && v1.own[k].at == o0.at && v1.own[k].copied == o0.copied);
                            assert(!this(me, inc, o0.id)(e));
                            assert(c1 == c0);
                        }
                    }
                    assert(unique_own(v1.own)) by {
                        assert forall|a: int, b: int| 0 <= a < b < v1.own.len() implies v1.own[a].id != v1.own[b].id by {
                            assert(v0.own[a].id != v0.own[b].id);
                        }
                    }
                    lemma_ocount_same(v0.own, v1.own);
                    assert forall|k: int| 0 <= k < self.own@.len() implies (#[trigger] self.own@[k]).id == prev.own@[k].id by {
                        assert(before.own@[k].id == prev.own@[k].id);
                    }
                    assert forall|k: int| 0 <= k < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[k]).id)) by {
                        assert(before.own@[k].id == self.own@[k].id);
                    }
                    assert(v1.pending.subrange(0, removed.pending@.len() as int) =~= removed.pending@);
                    assert(v1.relayed.subrange(0, removed.relayed@.len() as int) =~= removed.relayed@);
                    assert forall|k| holds(prev.v(), k) implies holds(v1, k) by {
                        if holds(v0, k) {
                            lemma_holds_fwd_remove(v0, j as int, k);
                            if k == key(e) {
                                if aside { assert(key(v1.pending[v1.pending.len() - 1]) == k); } else { assert(key(v1.relayed[v1.relayed.len() - 1]) == k); }
                            } else {
                                lemma_holds_extend(removed.v(), v1, k);
                            }
                        }
                    }
                    assert(waits(self.batches@, 0) == waits(prev.batches@, 0));
                }
            }
            i += 1;
        }
        proof {
            assert forall|id: u64| has_own(self.v(), id) implies has_own(prev.v(), id) by {
                let k = choose|k: int| 0 <= k < self.own@.len() && self.own@[k].id == id;
                assert(prev.own@[k].id == id);
            }
            assert forall|id: u64| has_own(prev.v(), id) implies has_own(self.v(), id) by {
                let k = choose|k: int| 0 <= k < prev.own@.len() && prev.own@[k].id == id;
                assert(self.own@[k].id == id);
            }
        }
        hedged
    }

    /// A snapshot through `next - 1` was installed with `dedup`: the own entries it contains
    /// are resolved (returned), and local batches in covered slots are proposed again.
    /// DEV: the records for the `Left`s; the ids collected first, then resolved.
    #[verifier::rlimit(300)]
    pub fn installed_snapshot(&mut self, next: u64, dedup: &Dedup) -> (left: Vec<Own>)
        requires old(self).inv(), held(old(self).v()) <= held_max(),
        ensures
            final(self).ready(*dedup), final(self).me == old(self).me, final(self).inc == old(self).inc,
            final(self).next_batch == old(self).next_batch,
            // E2: own records leave only as returned, exactly those the snapshot committed.
            forall|id: u64| has_own(old(self).v(), id) && !dedup.has((old(self).me, old(self).inc, id)) ==> has_own(final(self).v(), id),
            forall|id: u64| has_own(final(self).v(), id) ==> has_own(old(self).v(), id),
            forall|a: int| 0 <= a < left@.len() ==> has_own(old(self).v(), (#[trigger] left@[a]).id) && dedup.has((old(self).me, old(self).inc, left@[a].id)),
            // E3
            forall|k| holds(old(self).v(), k) ==> holds(final(self).v(), k) || dedup.has(k),
            forall|b: int| 0 <= b < final(self).batches@.len() ==> match (#[trigger] final(self).batches@[b]).slot { Some(s) => s >= next, None => true },
    {
        let ghost prev = *self;
        let (me, inc) = (self.me, self.inc);
        proof { self.dedup = Ghost(dedup.keys@.to_set()); }
        self.prune_due = true;
        let ghost mid0 = *self;
        let mut done: Vec<u64> = Vec::new();
        let mut k = 0;
        while k < self.own.len()
            invariant
                k <= self.own@.len(), *self == mid0, mid0.own == prev.own,
                forall|a: int| 0 <= a < done@.len() ==> dedup.has((me, inc, #[trigger] done@[a])) && has_own(prev.v(), done@[a]),
                forall|a: int| 0 <= a < k && dedup.has((me, inc, (#[trigger] self.own@[a]).id)) ==> done@.contains(self.own@[a].id),
            decreases self.own.len() - k,
        {
            if dedup.contains(me, inc, self.own[k].id) {
                let ghost d0 = done@;
                done.push(self.own[k].id);
                proof {
                    assert(prev.own@[k as int].id == self.own@[k as int].id);
                    assert(done@[d0.len() as int] == self.own@[k as int].id);
                    assert forall|a: int| 0 <= a < k && dedup.has((me, inc, (#[trigger] self.own@[a]).id)) implies done@.contains(self.own@[a].id) by {
                        let b = choose|b: int| 0 <= b < d0.len() && d0[b] == self.own@[a].id;
                        assert(done@[b] == d0[b]);
                    }
                    assert forall|a: int| 0 <= a < done@.len() implies dedup.has((me, inc, #[trigger] done@[a])) && has_own(prev.v(), done@[a]) by {
                        if a < d0.len() { assert(done@[a] == d0[a]); } else { assert(prev.v().own[k as int].id == done@[a]); }
                    }
                }
            }
            k += 1;
        }
        let mut left: Vec<Own> = Vec::new();
        let mut i = 0;
        proof {
            assert forall|id: u64| has_own(self.v(), id) && dedup.has((me, inc, id)) implies
                exists|a: int| 0 <= a < done@.len() && done@[a] == id by {
                let kk = choose|kk: int| 0 <= kk < self.own@.len() && self.own@[kk].id == id;
                assert(done@.contains(self.own@[kk].id));
            }
        }
        while i < done.len()
            invariant
                i <= done@.len(), self.inv(), self.me == me, self.inc == inc, self.dedup == mid0.dedup,
                self.prune_due, self.next_batch == prev.next_batch,
                forall|kk| dedup.has(kk) <==> self.dedup@.contains(kk),
                forall|a: int| 0 <= a < done@.len() ==> dedup.has((me, inc, #[trigger] done@[a])) && has_own(prev.v(), done@[a]),
                forall|id: u64| has_own(self.v(), id) ==> has_own(prev.v(), id),
                forall|id: u64| has_own(prev.v(), id) && !dedup.has((me, inc, id)) ==> has_own(self.v(), id),
                forall|id: u64| has_own(self.v(), id) && dedup.has((me, inc, id)) ==> exists|a: int| i <= a < done@.len() && done@[a] == id,
                forall|a: int| 0 <= a < left@.len() ==> has_own(prev.v(), (#[trigger] left@[a]).id) && dedup.has((me, inc, left@[a].id)),
                forall|kk| holds(prev.v(), kk) ==> holds(self.v(), kk) || dedup.has(kk),
                held(self.v()) <= held(prev.v()),
            decreases done.len() - i,
        {
            let id = done[i];
            let ghost before = *self;
            proof { assert(self.dedup@.contains((me, inc, id))); }
            match self.resolved(id) {
                Some(o) => {
                    let ghost l0 = left@;
                    left.push(o);
                    proof {
                        assert forall|a: int| 0 <= a < left@.len() implies has_own(prev.v(), (#[trigger] left@[a]).id) && dedup.has((me, inc, left@[a].id)) by {
                            if a < l0.len() { assert(left@[a] == l0[a]); }
                        }
                    }
                },
                None => {},
            }
            proof {
                assert forall|x: u64| has_own(self.v(), x) && dedup.has((me, inc, x)) implies exists|a: int| i + 1 <= a < done@.len() && done@[a] == x by {
                    let a = choose|a: int| i <= a < done@.len() && done@[a] == x;
                    assert(a != i);
                }
                assert forall|kk| holds(prev.v(), kk) implies holds(self.v(), kk) || dedup.has(kk) by {
                    if holds(before.v(), kk) {}
                }
            }
            i += 1;
        }
        proof {
            assert forall|kk: int| 0 <= kk < self.own@.len() implies !self.dedup@.contains((me, inc, (#[trigger] self.own@[kk]).id)) by {
                assert(has_own(self.v(), self.own@[kk].id));
            }
        }
        let mut j = 0;
        while j < self.batches.len()
            invariant
                j <= self.batches@.len(), self.inv(), self.settled(), self.me == me, self.inc == inc,
                self.dedup == mid0.dedup, self.prune_due, self.next_batch == prev.next_batch,
                forall|kk| dedup.has(kk) <==> self.dedup@.contains(kk),
                forall|id: u64| has_own(self.v(), id) ==> has_own(prev.v(), id),
                forall|id: u64| has_own(prev.v(), id) && !dedup.has((me, inc, id)) ==> has_own(self.v(), id),
                forall|kk| holds(prev.v(), kk) ==> holds(self.v(), kk) || dedup.has(kk),
                held(self.v()) <= held(prev.v()), held(prev.v()) <= held_max(),
                forall|b: int| 0 <= b < j ==> match (#[trigger] self.batches@[b]).slot { Some(s) => s >= next, None => true },
            decreases self.batches.len() - j,
        {
            let stale = match self.batches[j].slot {
                Some(s) => s < next,
                None => false,
            };
            if stale {
                let ghost before = *self;
                self.unslot(j);
                proof {
                    assert forall|b: int| 0 <= b < j implies match (#[trigger] self.batches@[b]).slot { Some(s) => s >= next, None => true } by {
                        assert(self.batches@[b].slot == before.batches@[b].slot);
                    }
                    assert forall|kk| holds(prev.v(), kk) implies holds(self.v(), kk) || dedup.has(kk) by {
                        if holds(before.v(), kk) {}
                    }
                    assert forall|id: u64| has_own(prev.v(), id) && !dedup.has((me, inc, id)) implies has_own(self.v(), id) by {
                        assert(has_own(before.v(), id));
                    }
                }
            }
            j += 1;
        }
        left
    }

    /// Fenced: everything is dropped; returns the own records that were awaited.
    pub fn fence(&mut self) -> (left: Vec<Own>)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).settled(), left@ == old(self).own@,
            final(self).own@.len() == 0, final(self).pending@.len() == 0, final(self).relayed@.len() == 0,
            final(self).forwarded@.len() == 0, final(self).batches@.len() == 0,
    {
        let mut own: Vec<Own> = Vec::new();
        std::mem::swap(&mut self.own, &mut own);
        self.pending.clear();
        self.relayed.clear();
        self.forwarded.clear();
        self.batches.clear();
        self.unslotted.clear();
        self.foreign = 0;
        self.changes = 0;
        self.prune_due = false;
        proof {
            assert(views(self.batches@) =~= Seq::<BatchV>::empty());
            assert(count(self.pending@, change()) == 0);
            assert(count(self.relayed@, change()) == 0);
            assert(count(self.relayed@, foreign(self.me)) == 0);
            assert(bsum(views(self.batches@), change()) == 0);
            assert(bsum(views(self.batches@), foreign(self.me)) == 0);
            assert(ocount(self.own@) == 0);
            assert forall|s: u64| self.unslotted@.contains(s) <==> waits(self.batches@, s) by {}
        }
        own
    }

    /// The first batch with entries waiting for a slot. DEV: a scan for `first()`.
    pub fn waiting(&self) -> (r: Option<u64>)
        requires self.inv(),
        ensures
            r is None <==> !(exists|s: u64| waits(self.batches@, s)),
            r matches Some(s) ==> waits(self.batches@, s) && forall|t: u64| waits(self.batches@, t) ==> s <= t,
    {
        if self.unslotted.len() == 0 {
            proof {
                if exists|s: u64| waits(self.batches@, s) {
                    let s = choose|s: u64| waits(self.batches@, s);
                    assert(self.unslotted@.contains(s));
                }
            }
            return None;
        }
        let mut m = self.unslotted[0];
        let mut i = 1;
        while i < self.unslotted.len()
            invariant
                1 <= i <= self.unslotted@.len(),
                exists|a: int| 0 <= a < i && self.unslotted@[a] == m,
                forall|a: int| 0 <= a < i ==> m <= self.unslotted@[a],
            decreases self.unslotted.len() - i,
        {
            if self.unslotted[i] < m {
                m = self.unslotted[i];
            }
            i += 1;
        }
        proof {
            let a = choose|a: int| 0 <= a < i && self.unslotted@[a] == m;
            assert(self.unslotted@.contains(m));
            assert forall|t: u64| waits(self.batches@, t) implies m <= t by {
                assert(self.unslotted@.contains(t));
                let b = choose|b: int| 0 <= b < self.unslotted@.len() && self.unslotted@[b] == t;
            }
        }
        Some(m)
    }

}

} // verus!
