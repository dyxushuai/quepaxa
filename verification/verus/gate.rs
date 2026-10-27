// crates/quepaxa/src/replica/gate.rs, ported to Verus: persist-before-send for every
// sequence of calls (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic line for line; records, messages and effects become
// u64 ids, and each send and effect carries a ghost `need`: how many writes must be durable
// before it may happen. `w` counts writes so far (the incarnation is write 1; pending writes
// count once, as they go out in one Persist), `cover[i]` is how many writes Persist i makes
// durable, and the durable count is cover[acked]. A send that reports state needs every
// write so far (`need == w`); a plain send needs only the incarnation (`need == 1`).
// Proved: every Send the gate emits and every effect it returns has need <= the durable
// count, and every Persist's `then` has need <= cover[id] (the runtime sends them only
// once that Persist is durable). Fencing is terminal: the replica stops calling the gate.
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
    fenced: Ghost<bool>,
}

pub open spec fn outs_le(s: Seq<Out>, n: nat) -> bool {
    forall|i: int| 0 <= i < s.len() ==> (#[trigger] s[i]).need@ <= n
}

pub open spec fn effs_le(s: Seq<Eff>, n: nat) -> bool {
    forall|i: int| 0 <= i < s.len() ==> (#[trigger] s[i]).need@ <= n
}

/// What an emitted action promises, given the persists issued so far and the durable count.
pub open spec fn action_ok(a: Action, cover: Seq<nat>, durable: nat) -> bool {
    match a {
        Action::Send(o) => o.need@ <= durable,
        Action::Persist { id, then, .. } => id < cover.len() && outs_le(then@, cover[id as int]),
    }
}

pub open spec fn actions_ok(s: Seq<Action>, from: int, cover: Seq<nat>, durable: nat) -> bool {
    forall|i: int| from <= i < s.len() ==> action_ok(#[trigger] s[i], cover, durable)
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
        &&& !self.fenced@
        &&& self.next_persist < u64::MAX
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
        &&& (self.writes@.len() == 0) == (self.w@ == self.issued())
        &&& self.w@ >= self.issued()
        &&& outs_le(self.gated@, self.w@)
        &&& effs_le(self.effects@, self.w@)
    }

    pub(crate) fn new() -> (g: Gate)
        ensures g.inv(), g.w@ == 1, g.durable() == 0,
    {
        Gate {
            next_persist: 0,
            acked: None,
            writes: vec![0],
            replace: false,
            gated: Vec::new(),
            effects: Vec::new(),
            in_flight: VecDeque::new(),
            w: Ghost(1),
            cover: Ghost(Seq::empty()),
            fenced: Ghost(false),
        }
    }

    fn ready(&self) -> (r: bool)
        requires self.inv(),
        ensures r ==> self.durable() >= 1,
    {
        self.acked.is_some()
    }

    fn idle(&self) -> (r: bool)
        requires self.inv(),
        ensures r ==> self.durable() == self.w@,
    {
        // DEV: `is_empty()` has no vstd spec on Vec or VecDeque.
        self.ready() && self.writes.len() == 0 && self.in_flight.len() == 0
    }

    pub(crate) fn write(&mut self, record: u64)
        requires old(self).inv(),
        ensures final(self).inv(), final(self).w@ >= old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
    {
        self.writes.push(record);
        if self.writes.len() == 1 {
            self.w = Ghost(self.w@ + 1);
        }
    }

    pub(crate) fn checkpoint(&mut self, records: Vec<u64>)
        requires old(self).inv(), records@.len() > 0,
        ensures final(self).inv(), final(self).w@ >= old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
    {
        self.writes = records;
        self.replace = true;
        self.w = Ghost(if old(self).w@ == old(self).issued() { old(self).w@ + 1 } else { old(self).w@ });
        assert(effs_le(self.effects@, self.w@));
        assert(outs_le(self.gated@, self.w@));
    }

    pub(crate) fn send(&mut self, out: Out, actions: &mut Vec<Action>)
        requires old(self).inv(), out.need@ == 1,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).durable()),
        ensures final(self).inv(), final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).durable()),
    {
        if self.ready() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    pub(crate) fn send_after_durable(&mut self, out: Out, actions: &mut Vec<Action>)
        requires old(self).inv(), out.need@ == old(self).w@,
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).durable()),
        ensures final(self).inv(), final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).durable()),
    {
        if self.idle() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    pub(crate) fn hold(&mut self, out: Out)
        requires old(self).inv(), out.need@ == old(self).w@,
        ensures final(self).inv(), final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(), final(self).cover@ == old(self).cover@,
    {
        self.gated.push(out);
    }

    pub(crate) fn after_durable(&mut self, e: Eff) -> (r: Option<Eff>)
        requires old(self).inv(), e.need@ == old(self).w@,
        ensures final(self).inv(), final(self).w@ == old(self).w@,
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
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).durable()),
        ensures final(self).inv(), final(self).w@ == old(self).w@,
            final(self).durable() == old(self).durable(),
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).durable()),
    {
        if self.writes.len() == 0 {
            self.park();
            return;
        }
        let id = self.next_persist;
        self.next_persist = self.next_persist + 1;
        self.cover = Ghost(self.cover@.push(self.w@));
        // DEV: `mem::take` has no vstd spec; `mem::swap` does.
        let mut records = Vec::new();
        std::mem::swap(&mut records, &mut self.writes);
        let mut then = Vec::new();
        std::mem::swap(&mut then, &mut self.gated);
        let replace = self.replace;
        self.replace = false;
        let mut effects = Vec::new();
        std::mem::swap(&mut effects, &mut self.effects);
        assert(actions_ok(actions@, 0, self.cover@, self.durable())) by {
            assert forall|i: int| 0 <= i < actions@.len() implies
                action_ok(#[trigger] actions@[i], self.cover@, self.durable()) by {
                assert(action_ok(actions@[i], old(self).cover@, old(self).durable()));
            }
        }
        actions.push(Action::Persist { id, records, then, replace });
        self.in_flight.push_back(InFlight { id, effects, parked: Vec::new() });
        assert(self.in_flight@.last().id == self.base() + (self.in_flight@.len() - 1));
    }

    fn park(&mut self)
        requires old(self).inv(),
        ensures final(self).inv(), final(self).w@ == old(self).w@,
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
            // At this point w == issued == cover[last], so the moved work fits the last persist.
            self.in_flight[n - 1].effects.append(&mut effects);
            self.in_flight[n - 1].parked.append(&mut gated);
        }
    }

    pub(crate) fn persisted(&mut self, id: u64, actions: &mut Vec<Action>) -> (r: Result<Vec<Eff>, ()>)
        requires old(self).inv(),
            actions_ok(old(actions)@, 0, old(self).cover@, old(self).durable()),
        ensures final(self).inv(), final(self).w@ == old(self).w@,
            final(self).cover@ == old(self).cover@,
            final(self).durable() >= old(self).durable(),
            actions_ok(final(actions)@, 0, final(self).cover@, final(self).durable()),
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
                action_ok(#[trigger] actions@[i], self.cover@, d) by {
                assert(action_ok(actions@[i], self.cover@, d0));
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
                self.w@ == old(self).w@,
                !self.fenced@,
                (self.writes@.len() == 0) == (self.w@ == self.issued()),
                outs_le(self.gated@, self.w@),
                effs_le(self.effects@, self.w@),
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
                actions_ok(actions@, 0, self.cover@, d),
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
                    actions_ok(actions@, 0, self.cover@, d),
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
        ensures final(self).fenced@,
    {
        self.writes.clear();
        self.gated.clear();
        self.effects.clear();
        self.fenced = Ghost(true);
    }
}


} // verus!
