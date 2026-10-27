// crates/quepaxa/src/replica/gate.rs, ported to Verus: persist-before-send for every
// sequence of calls (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic statement by statement, except where marked DEV.
// Records, messages and effects become u64 ids, and each send and effect carries a ghost
// `need`: how many writes must be durable before it may happen. `w` counts writes so far
// (the incarnation is write 1; pending writes count once, as they go out in one Persist),
// `cover[i]` is how many writes Persist i makes durable, and the durable count is
// cover[acked]. A send that reports state needs every write so far (`need == w`); a plain
// send needs only the incarnation (`need == 1`).
//
// Proved, for every sequence of calls the replica makes (fewer than u64::MAX - 1 persists):
// - every Send the gate emits and every effect it returns has need <= the durable count,
//   and every Persist's `then` has need <= cover[id] (sent only once that Persist is
//   durable);
// - the Persists carry exactly the records written: replaying them in order (appending,
//   or replacing for a checkpoint) and then the writes not yet issued gives `log`, the
//   records every write and checkpoint call asked for. That a checkpoint's records reflect
//   everything before it is the caller's contract, not proved here.
// After `fence` (storage failed) the replica still calls `send` and `flush` in the same
// step, and nothing else; the same rules hold for them, and nothing waiting is released.
//
// Departures from the original's code, forced by what vstd specifies, are marked DEV;
// VERIFICATION.md lists them. When gate.rs changes, port the change here.
use vstd::prelude::*;
use std::collections::VecDeque;

verus! {

pub struct Out {
    pub to: u64,
    pub need: Ghost<nat>,
}

pub struct Eff {
    pub id: u64,
    pub need: Ghost<nat>,
}

pub enum Action {
    Send(Out),
    Persist { id: u64, records: Vec<u64>, then: Vec<Out>, replace: bool },
}

pub struct InFlight {
    pub id: u64,
    pub effects: Vec<Eff>,
    pub parked: Vec<Out>,
}

pub(crate) struct Gate {
    next_persist: u64,
    acked: Option<u64>,
    writes: Vec<u64>,
    replace: bool,
    gated: Vec<Out>,
    effects: Vec<Eff>,
    in_flight: VecDeque<InFlight>,
    w: Ghost<nat>,
    cover: Ghost<Seq<nat>>,
    log: Ghost<Seq<u64>>,
    recs: Ghost<Seq<(Seq<u64>, bool)>>,
    fenced: Ghost<bool>,
}

/// What storage holds once the first `n` Persists are durable.
pub open spec fn store(recs: Seq<(Seq<u64>, bool)>, n: nat) -> Seq<u64>
    decreases n,
{
    if n == 0 || n > recs.len() {
        Seq::empty()
    } else if recs[n - 1].1 {
        recs[n - 1].0
    } else {
        store(recs, (n - 1) as nat) + recs[n - 1].0
    }
}

pub open spec fn outs_le(s: Seq<Out>, n: nat) -> bool {
    forall|i: int| 0 <= i < s.len() ==> (#[trigger] s[i]).need@ <= n
}

pub open spec fn effs_le(s: Seq<Eff>, n: nat) -> bool {
    forall|i: int| 0 <= i < s.len() ==> (#[trigger] s[i]).need@ <= n
}

/// What an emitted action promises, given the persists issued so far and the durable count.
pub open spec fn action_ok(a: Action, cover: Seq<nat>, recs: Seq<(Seq<u64>, bool)>, durable: nat) -> bool {
    match a {
        Action::Send(o) => o.need@ <= durable,
        Action::Persist { id, records, then, replace } => {
            &&& id < cover.len() && id < recs.len()
            &&& outs_le(then@, cover[id as int])
            &&& records@ == recs[id as int].0 && replace == recs[id as int].1
        },
    }
}

pub open spec fn actions_ok(s: Seq<Action>, from: int, cover: Seq<nat>, recs: Seq<(Seq<u64>, bool)>, durable: nat) -> bool {
    forall|i: int| from <= i < s.len() ==> action_ok(#[trigger] s[i], cover, recs, durable)
}

/// Persists past the first `n` do not change what the first `n` store.
proof fn store_prefix(recs: Seq<(Seq<u64>, bool)>, pre: Seq<(Seq<u64>, bool)>, n: nat)
    requires n <= pre.len() <= recs.len(), pre =~= recs.subrange(0, pre.len() as int),
    ensures store(recs, n) == store(pre, n),
    decreases n,
{
    if n > 0 {
        assert(pre[n - 1] == recs[n - 1]);
        store_prefix(recs, pre, (n - 1) as nat);
    }
}

impl Gate {
    spec fn base(self) -> nat {
        match self.acked { Some(a) => a as nat + 1, None => 0 }
    }

    spec fn durable(self) -> nat {
        match self.acked { Some(a) => self.cover@[a as int], None => 0 }
    }

    spec fn issued(self) -> nat {
        if self.next_persist == 0 { 0 } else { self.cover@[self.next_persist - 1] }
    }

    spec fn inv(self) -> bool {
        &&& self.next_persist < u64::MAX
        &&& self.recs@.len() == self.next_persist
        &&& self.cover@.len() == self.next_persist
        &&& self.w@ >= 1
        &&& forall|i: int| 0 <= i < self.cover@.len() ==> 1 <= #[trigger] self.cover@[i] <= self.w@
        &&& forall|i: int, j: int| 0 <= i <= j < self.cover@.len() ==> self.cover@[i] <= self.cover@[j]
        &&& match self.acked { Some(a) => a < self.next_persist, None => true }
        &&& self.in_flight@.len() == self.next_persist - self.base()
        &&& forall|k: int| 0 <= k < self.in_flight@.len() ==> {
                &&& (#[trigger] self.in_flight@[k]).id == self.base() + k
                &&& outs_le(self.in_flight@[k].parked@, self.cover@[self.base() + k])
                &&& effs_le(self.in_flight@[k].effects@, self.cover@[self.base() + k])
            }
        &&& self.w@ >= self.issued()
        &&& if self.fenced@ {
                // Fenced: nothing waits for a write, and only plain sends wait for the
                // incarnation.
                &&& self.writes@.len() == 0 && self.effects@.len() == 0
                &&& outs_le(self.gated@, 1)
            } else {
                &&& (self.writes@.len() == 0) == (self.w@ == self.issued())
                &&& outs_le(self.gated@, self.w@)
                &&& effs_le(self.effects@, self.w@)
                &&& self.replace ==> self.writes@.len() > 0
                &&& self.log@ == if self.replace { self.writes@ }
                        else { store(self.recs@, self.next_persist as nat) + self.writes@ }
            }
    }

    pub(crate) fn new() -> (g: Gate)
        ensures g.inv(), !g.fenced@, g.w@ == 1, g.durable() == 0, g.log@ == Seq::empty().push(0u64),
    {
        let g = Gate {
            next_persist: 0,
            acked: None,
            writes: vec![0],
            replace: false,
            gated: Vec::new(),
            effects: Vec::new(),
            in_flight: VecDeque::new(),
            w: Ghost(1),
            cover: Ghost(Seq::empty()),
            log: Ghost(Seq::empty().push(0u64)),
            recs: Ghost(Seq::empty()),
            fenced: Ghost(false),
        };
        assert(g.log@ =~= store(g.recs@, 0) + g.writes@);
        g
    }

    fn ready(&self) -> (r: bool)
        requires self.inv(),
        ensures r ==> self.durable() >= 1,
    {
        self.acked.is_some()
    }

    fn idle(&self) -> (r: bool)
        requires self.inv(), !self.fenced@,
        ensures r ==> self.durable() == self.w@,
    {
        // DEV: `is_empty()` has no vstd spec on Vec or VecDeque.
        self.ready() && self.writes.len() == 0 && self.in_flight.len() == 0
    }

    pub(crate) fn write(&mut self, record: u64)
        requires old(self).inv(), !old(self).fenced@,
        ensures final(self).inv(), !final(self).fenced@, final(self).w@ >= old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            final(self).recs@ == old(self).recs@, final(self).log@ == old(self).log@.push(record),
    {
        self.log = Ghost(self.log@.push(record));
        self.writes.push(record);
        if self.writes.len() == 1 {
            self.w = Ghost(self.w@ + 1);
        }
    }

    pub(crate) fn checkpoint(&mut self, records: Vec<u64>)
        requires old(self).inv(), !old(self).fenced@, records@.len() > 0,
        ensures final(self).inv(), !final(self).fenced@, final(self).w@ >= old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            final(self).recs@ == old(self).recs@, final(self).log@ == records@,
    {
        self.log = Ghost(records@);
        self.writes = records;
        self.replace = true;
        self.w = Ghost(if old(self).w@ == old(self).issued() { old(self).w@ + 1 } else { old(self).w@ });
        assert(effs_le(self.effects@, self.w@));
        assert(outs_le(self.gated@, self.w@));
    }

    pub(crate) fn send(&mut self, out: Out, actions: &mut Vec<Action>)
        requires old(self).inv(), out.need@ == 1,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).recs@, old(self).durable()),
        ensures final(self).inv(), final(self).fenced@ == old(self).fenced@,
            final(self).recs@ == old(self).recs@, final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).recs@, final(self).durable()),
    {
        if self.ready() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    pub(crate) fn send_after_durable(&mut self, out: Out, actions: &mut Vec<Action>)
        requires old(self).inv(), !old(self).fenced@, out.need@ == old(self).w@,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).recs@, old(self).durable()),
        ensures final(self).inv(), !final(self).fenced@, final(self).recs@ == old(self).recs@,
            final(self).log@ == old(self).log@, final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).recs@, final(self).durable()),
    {
        if self.idle() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    pub(crate) fn hold(&mut self, out: Out)
        requires old(self).inv(), !old(self).fenced@, out.need@ == old(self).w@,
        ensures final(self).inv(), !final(self).fenced@, final(self).recs@ == old(self).recs@,
            final(self).log@ == old(self).log@, final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
    {
        // DEV: the original's `debug_assert!(!self.idle())` is left out (debug-only).
        self.gated.push(out);
    }

    pub(crate) fn after_durable(&mut self, e: Eff) -> (r: Option<Eff>)
        requires old(self).inv(), !old(self).fenced@, e.need@ == old(self).w@,
        ensures final(self).inv(), !final(self).fenced@, final(self).recs@ == old(self).recs@,
            final(self).log@ == old(self).log@, final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            r matches Some(x) ==> x.need@ <= final(self).durable(),
    {
        if self.idle() {
            Some(e)
        } else {
            self.effects.push(e);
            None
        }
    }

    pub(crate) fn flush(&mut self, actions: &mut Vec<Action>)
        requires old(self).inv(), old(self).next_persist + 1 < u64::MAX,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).recs@, old(self).durable()),
        ensures final(self).inv(), final(self).fenced@ == old(self).fenced@,
            final(self).log@ == old(self).log@, final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(),
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).recs@, final(self).durable()),
    {
        if self.writes.len() == 0 {
            // DEV: the original's `debug_assert!` that no work waits without a write is left
            // out (debug-only).
            self.park();
            return;
        }
        let id = self.next_persist;
        self.next_persist = self.next_persist + 1;
        self.cover = Ghost(self.cover@.push(self.w@));
        self.recs = Ghost(self.recs@.push((self.writes@, self.replace)));
        // DEV: `mem::take` has no vstd spec; `mem::swap` does.
        let mut records = Vec::new();
        std::mem::swap(&mut records, &mut self.writes);
        let mut then = Vec::new();
        std::mem::swap(&mut then, &mut self.gated);
        let replace = self.replace;
        self.replace = false;
        let mut effects = Vec::new();
        std::mem::swap(&mut effects, &mut self.effects);
        assert(actions_ok(actions@, 0, self.cover@, self.recs@, self.durable())) by {
            assert forall|i: int| 0 <= i < actions@.len() implies
                action_ok(#[trigger] actions@[i], self.cover@, self.recs@, self.durable()) by {
                assert(action_ok(actions@[i], old(self).cover@, old(self).recs@, old(self).durable()));
            }
        }
        assert(store(self.recs@, self.next_persist as nat) == old(self).log@) by {
            assert(self.recs@[self.next_persist - 1] == (records@, replace));
            assert(old(self).recs@ =~= self.recs@.subrange(0, old(self).next_persist as int));
            store_prefix(self.recs@, old(self).recs@, old(self).next_persist as nat);
        }
        assert(self.log@ == store(self.recs@, self.next_persist as nat) + self.writes@);
        actions.push(Action::Persist { id, records, then, replace });
        self.in_flight.push_back(InFlight { id, effects, parked: Vec::new() });
        assert(self.in_flight@.last().id == self.base() + (self.in_flight@.len() - 1));
    }

    fn park(&mut self)
        requires old(self).inv(),
        ensures final(self).inv(), final(self).fenced@ == old(self).fenced@,
            final(self).recs@ == old(self).recs@, final(self).log@ == old(self).log@,
            final(self).writes@ == old(self).writes@, final(self).replace == old(self).replace,
            final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            final(self).acked == old(self).acked, final(self).next_persist == old(self).next_persist,
    {
        // DEV: `back_mut()` has no vstd spec; index the last element instead.
        let n = self.in_flight.len();
        if self.writes.len() == 0 && n > 0 {
            let mut effects = Vec::new();
            std::mem::swap(&mut effects, &mut self.effects);
            let mut gated = Vec::new();
            std::mem::swap(&mut gated, &mut self.gated);
            let ghost k = n - 1;
            assert(self.in_flight@[k].id == self.base() + k);
            // Unfenced, w == issued == cover[last]; fenced, what waits needs only write 1. So
            // the moved work fits the last persist either way.
            assert(self.cover@[k + self.base()] >= 1);
            self.in_flight[n - 1].effects.append(&mut effects);
            self.in_flight[n - 1].parked.append(&mut gated);
        }
    }

    pub(crate) fn persisted(&mut self, id: u64, actions: &mut Vec<Action>) -> (r: Result<Vec<Eff>, ()>)
        requires old(self).inv(), !old(self).fenced@,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).recs@, old(self).durable()),
        ensures final(self).inv(), !final(self).fenced@, final(self).w@ == old(self).w@,
            final(self).cover@ == old(self).cover@, final(self).recs@ == old(self).recs@,
            final(self).log@ == old(self).log@,
            final(self).durable() >= old(self).durable(),
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).recs@, final(self).durable()),
            r matches Ok(es) ==> effs_le(es@, final(self).durable()),
    {
        let stale = match self.acked { Some(a) => id <= a, None => false };
        if id >= self.next_persist || stale {
            return Err(());
        }
        // DEV: park before recording the ack (park reads neither `acked` nor the ids), so
        // that it runs on a state satisfying the invariant.
        self.park();
        let ghost b0 = self.base();
        let ghost q0 = self.in_flight@;
        let ghost d = self.cover@[id as int];
        let ghost d0 = self.durable();
        self.acked = Some(id);
        proof {
            assert forall|i: int| 0 <= i < actions@.len() implies
                action_ok(#[trigger] actions@[i], self.cover@, self.recs@, d) by {
                assert(action_ok(actions@[i], self.cover@, self.recs@, d0));
            }
        }
        let mut effects: Vec<Eff> = Vec::new();
        let ghost mut p: int = 0;
        // DEV: `while let Some(..) = front()` and `extend(map(Send))` become index loops
        // (no vstd spec for `front` or iterator adapters).
        loop
            invariant
                self.acked == Some(id),
                self.next_persist == old(self).next_persist,
                self.cover@ == old(self).cover@,
                self.recs@ == old(self).recs@,
                self.log@ == old(self).log@,
                self.w@ == old(self).w@,
                !self.fenced@,
                (self.writes@.len() == 0) == (self.w@ == self.issued()),
                outs_le(self.gated@, self.w@),
                effs_le(self.effects@, self.w@),
                self.replace ==> self.writes@.len() > 0,
                self.log@ == if self.replace { self.writes@ }
                    else { store(self.recs@, self.next_persist as nat) + self.writes@ },
                old(self).inv(),
                b0 <= id < self.next_persist,
                q0.len() == self.next_persist - b0,
                forall|j: int| 0 <= j < q0.len() ==> {
                    &&& (#[trigger] q0[j]).id == b0 + j
                    &&& outs_le(q0[j].parked@, self.cover@[b0 + j])
                    &&& effs_le(q0[j].effects@, self.cover@[b0 + j])
                },
                0 <= p <= id + 1 - b0,
                self.in_flight@.len() + p == q0.len(),
                forall|k: int| 0 <= k < self.in_flight@.len() ==> #[trigger] self.in_flight@[k] == q0[p + k],
                d == self.cover@[id as int],
                effs_le(effects@, d),
                actions_ok(actions@, 0, self.cover@, self.recs@, d),
            ensures
                self.in_flight@.len() == 0 || self.in_flight@[0].id > id,
            decreases self.in_flight@.len(),
        {
            if self.in_flight.len() == 0 || self.in_flight[0].id > id {
                break;
            }
            assert(self.in_flight@[0] == q0[p]);
            assert(self.cover@[b0 + p] <= d);
            let done = self.in_flight.pop_front().unwrap();
            let mut parked = done.parked;
            while parked.len() > 0
                invariant
                    outs_le(parked@, d),
                    actions_ok(actions@, 0, self.cover@, self.recs@, d),
                    id < self.cover@.len(),
                decreases parked@.len(),
            {
                let o = parked.remove(0);
                actions.push(Action::Send(o));
            }
            let mut es = done.effects;
            effects.append(&mut es);
            proof { p = p + 1; }
            assert forall|k: int| 0 <= k < self.in_flight@.len() implies
                #[trigger] self.in_flight@[k] == q0[p + k] by {}
        }
        assert(p == id + 1 - b0) by {
            if self.in_flight@.len() > 0 {
                assert(self.in_flight@[0] == q0[p]);
            }
        }
        assert forall|k: int| 0 <= k < self.in_flight@.len() implies
            (#[trigger] self.in_flight@[k]).id == self.base() + k by {
            assert(self.in_flight@[k] == q0[p + k]);
        }
        assert(self.durable() >= d0);
        Ok(effects)
    }

    pub(crate) fn fence(&mut self)
        requires old(self).inv(), !old(self).fenced@,
        ensures final(self).inv(), final(self).fenced@, final(self).w@ == old(self).w@,
            final(self).cover@ == old(self).cover@, final(self).recs@ == old(self).recs@,
            final(self).durable() == old(self).durable(),
    {
        self.writes.clear();
        self.gated.clear();
        self.effects.clear();
        self.fenced = Ghost(true);
    }
}


} // verus!
