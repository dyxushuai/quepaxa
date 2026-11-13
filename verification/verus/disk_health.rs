// crates/quepaxa/src/replica/disk.rs, with the step-aside latch of watch.rs (`check_aside`)
// and the hold in `late_at`, ported to Verus (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic statement by statement, except where marked DEV.
// Timestamps are u64 nanoseconds. Ghost history records every event the replica feeds the
// disk: writes issued (id, time), writes completed (id, time: every write up to that id is
// durable), and decisions other replicas announced (time). Events come in time order.
//
// Proved, for every sequence of events (writes issued with ever larger ids, times below
// u64::MAX):
// (a) the disk is stuck (`Stuck` or `SharedStall`) exactly when a write is in flight, it has
//     been for δ (at least four usual write times), and no write completed in that span;
//     `SharedStall` exactly when, besides, no decision arrived after the oldest write in
//     flight was due (four usual write times after it was issued);
// (b) `late_at` holds an entry (no wake-up time) only while the disk is in a shared stall at
//     that moment, never on a forecast; time alone never ends a shared stall, and the write
//     completion that ends one records it (`resumed`), after which (δ above zero) every
//     entry has a wake-up time again, no earlier than the completion;
// (c) the step-aside latch alternates: every step aside is followed by a return before the
//     next, and it steps aside only while the disk is stuck alone (another replica decided
//     since the oldest write was issued) and that write has waited long enough.
//
// Departures from the original's code, forced by what vstd specifies, are marked DEV;
// VERIFICATION.md lists them. When disk.rs, `late_at` or `check_aside` change, port the
// change here.
use vstd::prelude::*;
use std::collections::VecDeque;

verus! {

/// `Timestamp::after`: saturating.
pub open spec fn after(t: u64, n: u64) -> u64 {
    if t as int + n as int > u64::MAX as int { u64::MAX } else { (t + n) as u64 }
}

pub open spec fn times4(x: u64) -> u64 {
    if x as int * 4 > u64::MAX as int { u64::MAX } else { (x * 4) as u64 }
}

pub open spec fn max(a: u64, b: u64) -> u64 {
    if a >= b { a } else { b }
}

// DEV: `saturating_add`, `saturating_mul(4)` and `max` spelled out (no vstd spec).
fn after_exec(t: u64, n: u64) -> (r: u64)
    ensures r == after(t, n),
{
    if t > u64::MAX - n { u64::MAX } else { t + n }
}

fn times4_exec(x: u64) -> (r: u64)
    ensures r == times4(x),
{
    if x > u64::MAX / 4 { u64::MAX } else { x * 4 }
}

fn max_exec(a: u64, b: u64) -> (r: u64)
    ensures r == max(a, b),
{
    if a >= b { a } else { b }
}

pub enum Health {
    Idle,
    Writing,
    Stuck { since: u64 },
    SharedStall { since: u64 },
}

pub struct Disk {
    pub writes: VecDeque<(u64, u64)>,
    pub usual: Option<u64>,
    pub wrote: u64,
    pub decided_elsewhere: u64,
    pub resumed: u64,
    pub issued: Ghost<Seq<(u64, u64)>>,
    pub done: Ghost<Seq<(u64, u64)>>,
    pub decided: Ghost<Seq<u64>>,
    /// The latest event's time.
    pub clock: Ghost<u64>,
    /// The largest id issued or completed, -1 before any.
    pub top: Ghost<int>,
}

impl Disk {
    /// Where the oldest write in flight is in the history.
    pub open spec fn k(self) -> int {
        self.issued@.len() - self.writes@.len()
    }

    pub open spec fn inv(self) -> bool {
        &&& self.writes@.len() <= self.issued@.len()
        &&& forall|i: int| 0 <= i < self.writes@.len() ==> #[trigger] self.writes@[i] == self.issued@[self.k() + i]
        &&& forall|i: int, j: int| #![trigger self.issued@[i], self.issued@[j]] 0 <= i < j < self.issued@.len() ==>
                self.issued@[i].0 < self.issued@[j].0 && self.issued@[i].1 <= self.issued@[j].1
        &&& forall|i: int| 0 <= i < self.issued@.len() ==>
                (#[trigger] self.issued@[i]).0 <= self.top@ && self.issued@[i].1 <= self.clock@
        &&& forall|j: int| 0 <= j < self.done@.len() ==>
                (#[trigger] self.done@[j]).0 <= self.top@ && self.done@[j].1 <= self.wrote
        // A write in flight was completed by no completion.
        &&& forall|i: int, j: int| #![trigger self.issued@[i], self.done@[j]]
                self.k() <= i < self.issued@.len() && 0 <= j < self.done@.len() ==> self.done@[j].0 < self.issued@[i].0
        &&& if self.done@.len() == 0 { self.wrote == 0 } else { self.wrote == self.done@.last().1 }
        &&& self.wrote <= self.clock@ && self.decided_elsewhere <= self.clock@
        &&& forall|j: int| 0 <= j < self.decided@.len() ==> #[trigger] self.decided@[j] <= self.decided_elsewhere
        &&& if self.decided@.len() == 0 { self.decided_elsewhere == 0 }
            else { self.decided_elsewhere == self.decided@.last() }
    }

    /// The oldest write in flight was issued at least `thr(d)` ago, and no write completed
    /// since `thr(d)` ago.
    pub open spec fn quiet(self, now: u64, d: u64) -> bool {
        match self.usual {
            None => false,
            Some(u) => {
                &&& self.writes@.len() > 0
                &&& after(self.issued@[self.k()].1, max(d, times4(u))) <= now
                &&& forall|j: int| 0 <= j < self.done@.len() ==>
                        after(#[trigger] self.done@[j].1, max(d, times4(u))) <= now
            },
        }
    }

    /// No decision arrived after the oldest write in flight was due.
    pub open spec fn nobody_decided(self) -> bool {
        match self.usual {
            None => false,
            Some(u) => forall|j: int| 0 <= j < self.decided@.len() ==>
                #[trigger] self.decided@[j] <= after(self.issued@[self.k()].1, times4(u)),
        }
    }

    pub open spec fn shared_stall(self, now: u64, d: u64) -> bool {
        self.quiet(now, d) && self.nobody_decided()
    }

    /// Another replica decided since the oldest write in flight was issued.
    pub open spec fn decided_since_issued(self) -> bool {
        exists|j: int| 0 <= j < self.decided@.len() && #[trigger] self.decided@[j] > self.issued@[self.k()].1
    }

    pub fn new() -> (s: Disk)
        ensures s.inv(), s.writes@.len() == 0,
    {
        Disk {
            writes: VecDeque::new(),
            usual: None,
            wrote: 0,
            decided_elsewhere: 0,
            resumed: 0,
            issued: Ghost(Seq::empty()),
            done: Ghost(Seq::empty()),
            decided: Ghost(Seq::empty()),
            clock: Ghost(0),
            top: Ghost(-1),
        }
    }

    pub fn issued(&mut self, id: u64, now: u64)
        requires old(self).inv(), id as int > old(self).top@, now >= old(self).clock@,
        ensures final(self).inv(), final(self).clock@ == now,
    {
        let ghost w0 = self.writes@;
        self.writes.push_back((id, now));
        self.issued = Ghost(self.issued@.push((id, now)));
        self.top = Ghost(id as int);
        self.clock = Ghost(now);
        assert(self.writes@ =~= w0.push((id, now)));
    }

    pub fn completed(&mut self, id: u64, now: u64, d: u64)
        requires old(self).inv(), now >= old(self).clock@, now < u64::MAX,
        ensures
            final(self).inv(), final(self).clock@ == now,
            old(self).shared_stall(now, d) ==> final(self).resumed == now,
            d > 0 ==> !final(self).quiet(now, d),
    {
        let h = self.health(now, d);
        if let Health::SharedStall { .. } = h {
            self.resumed = now;
        }
        let ghost done0 = self.done@;
        let ghost n = self.issued@.len();
        self.wrote = now;
        self.done = Ghost(self.done@.push((id, now)));
        self.clock = Ghost(now);
        self.top = Ghost(if id as int > self.top@ { id as int } else { self.top@ });
        // DEV: `while let Some(..) = front() && ..` becomes a loop over indexing.
        loop
            invariant
                self.issued@.len() == n,
                self.writes@.len() <= n,
                forall|i: int| 0 <= i < self.writes@.len() ==> #[trigger] self.writes@[i] == self.issued@[self.k() + i],
                forall|i: int, j: int| #![trigger self.issued@[i], self.issued@[j]] 0 <= i < j < n ==>
                    self.issued@[i].0 < self.issued@[j].0 && self.issued@[i].1 <= self.issued@[j].1,
                forall|i: int| 0 <= i < n ==> (#[trigger] self.issued@[i]).1 <= now,
                forall|i: int, j: int| #![trigger self.issued@[i], done0[j]] self.k() <= i < n && 0 <= j < done0.len() ==>
                    done0[j].0 < self.issued@[i].0,
            ensures
                self.writes@.len() == 0 || self.writes@[0].0 > id,
            decreases self.writes@.len(),
        {
            if self.writes.len() == 0 || self.writes[0].0 > id {
                break;
            }
            let ghost w0 = self.writes@;
            let (_, at) = self.writes.pop_front().unwrap();
            assert(self.writes@ =~= w0.subrange(1, w0.len() as int));
            assert forall|i: int| 0 <= i < self.writes@.len() implies
                #[trigger] self.writes@[i] == self.issued@[self.k() + i] by {
                assert(self.writes@[i] == w0[i + 1]);
            }
            // DEV: `saturating_sub` spelled out.
            let sample = if now >= at { now - at } else { 0 };
            self.usual = Some(match self.usual {
                None => sample,
                Some(avg) => {
                    assert(avg - avg / 8 + sample / 8 <= u64::MAX) by (nonlinear_arith)
                        requires avg <= u64::MAX, sample <= u64::MAX;
                    avg - avg / 8 + sample / 8
                },
            });
        }
        proof {
            // What is left is past `id`: ids grow along the queue.
            assert forall|i: int, j: int| #![trigger self.issued@[i], self.done@[j]]
                self.k() <= i < n && 0 <= j < self.done@.len() implies
                self.done@[j].0 < self.issued@[i].0 by {
                if j == done0.len() {
                    assert(self.writes@[0] == self.issued@[self.k()]);
                    if i > self.k() { assert(self.issued@[self.k()].0 < self.issued@[i].0); }
                } else {
                    assert(self.done@[j] == done0[j]);
                }
            }
            assert(self.done@.last() == (id, now));
            if self.usual is Some {
                if self.writes@.len() > 0 {
                    assert(self.done@[self.done@.len() - 1].1 == now);
                }
            }
        }
    }

    pub fn decided_elsewhere(&mut self, now: u64)
        requires old(self).inv(), now >= old(self).clock@,
        ensures final(self).inv(), final(self).clock@ == now,
    {
        self.decided_elsewhere = now;
        self.decided = Ghost(self.decided@.push(now));
        self.clock = Ghost(now);
    }

    pub fn health(&self, now: u64, d: u64) -> (r: Health)
        requires self.inv(),
        ensures
            match r {
                Health::Idle => self.writes@.len() == 0,
                Health::Writing => self.writes@.len() > 0 && !self.quiet(now, d),
                Health::Stuck { since } => since <= now && self.quiet(now, d) && !self.nobody_decided(),
                Health::SharedStall { since } => since <= now && self.shared_stall(now, d),
            },
    {
        if self.writes.len() == 0 {
            return Health::Idle;
        }
        let oldest = self.writes[0].1;
        proof { assert(self.writes@[0] == self.issued@[self.k()]); }
        let usual = match self.usual {
            Some(u) => u,
            None => return Health::Writing,
        };
        let usual4 = times4_exec(usual);
        let since = after_exec(max_exec(oldest, self.wrote), max_exec(d, usual4));
        if since > now {
            proof {
                if self.done@.len() > 0 && self.wrote > oldest {
                    assert(self.done@[self.done@.len() - 1].1 == self.wrote);
                }
            }
            Health::Writing
        } else if self.decided_elsewhere <= after_exec(oldest, usual4) {
            Health::SharedStall { since }
        } else {
            proof { assert(self.decided@[self.decided@.len() - 1] == self.decided_elsewhere); }
            Health::Stuck { since }
        }
    }

    pub fn alone_at(&self, wait: u64) -> (r: Option<u64>)
        requires self.inv(),
        ensures
            match r {
                Some(t) => self.writes@.len() > 0 && self.decided_since_issued() && match self.usual {
                    Some(u) => t == after(self.issued@[self.k()].1, max(wait, times4(u))),
                    None => false,
                },
                None => self.writes@.len() == 0 || self.usual is None || !self.decided_since_issued(),
            },
    {
        let usual = match self.usual {
            Some(u) => u,
            None => return None,
        };
        if self.writes.len() == 0 {
            return None;
        }
        let oldest = self.writes[0].1;
        proof { assert(self.writes@[0] == self.issued@[self.k()]); }
        if self.decided_elsewhere > oldest {
            proof { assert(self.decided@[self.decided@.len() - 1] == self.decided_elsewhere); }
            Some(after_exec(oldest, max_exec(wait, times4_exec(usual))))
        } else {
            None
        }
    }

    /// Time alone never ends a shared stall.
    pub proof fn stall_persists(self, t1: u64, t2: u64, d: u64)
        requires self.shared_stall(t1, d), t1 <= t2,
        ensures self.shared_stall(t2, d),
    {
    }
}

/// `Replica::late_at` for an entry forwarded at `at`: `wait` is δ or 4δ, `aside` the latch.
pub fn late_at(disk: &Disk, at: u64, now: u64, d: u64, wait: u64, aside: bool) -> (r: Option<u64>)
    requires disk.inv(),
    ensures
        r is None ==> !aside && disk.shared_stall(now, d),
        r matches Some(t) ==> t == after(max(at, disk.resumed), wait) && t >= disk.resumed,
{
    let late = after_exec(max_exec(at, disk.resumed), wait);
    let held = !aside && match disk.health(now, d) {
        Health::SharedStall { since } => since <= late,
        _ => false,
    };
    if held { None } else { Some(late) }
}

/// The step-aside latch: `Watch::aside` and `check_aside`.
pub struct Latch {
    pub aside: bool,
    pub steps: Ghost<nat>,
    pub returns: Ghost<nat>,
}

impl Latch {
    pub open spec fn inv(self) -> bool {
        if self.aside { self.steps@ == self.returns@ + 1 } else { self.steps@ == self.returns@ }
    }

    pub fn new() -> (l: Latch)
        ensures l.inv(), !l.aside,
    {
        Latch { aside: false, steps: Ghost(0), returns: Ghost(0) }
    }

    /// `leads`: others hand this replica their entries (`aside_at`'s role check); `wait` is
    /// 4δ, at most `hedge_max`, at least δ. DEV: handing the batches on is not ported.
    pub fn check_aside(&mut self, disk: &Disk, leads: bool, now: u64, wait: u64)
        requires old(self).inv(), disk.inv(),
        ensures
            final(self).inv(),
            final(self).steps@ >= old(self).steps@ && final(self).returns@ >= old(self).returns@,
            final(self).aside ==> leads && disk.writes@.len() > 0 && disk.decided_since_issued(),
            final(self).aside ==> match disk.usual {
                Some(u) => after(disk.issued@[disk.k()].1, max(wait, times4(u))) <= now,
                None => false,
            },
    {
        let aside = leads && match disk.alone_at(wait) {
            Some(t) => t <= now,
            None => false,
        };
        if aside == self.aside {
            return;
        }
        self.aside = aside;
        if !aside {
            self.returns = Ghost(self.returns@ + 1);
            return;
        }
        self.steps = Ghost(self.steps@ + 1);
    }
}

} // verus!
