// crates/quepaxa/src/replica/recovery.rs (`Phase`, `CatchUp`, `Reservation`), with
// `read_catch_up_from` (reads.rs) and the lines of `Replica::start`, `send_requests`,
// `apply`, `fire_timers` and `checkpoint` that use them, ported to Verus
// (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic statement by statement, except where marked DEV.
// Node ids, slots and timestamps are u64. The durability gate is modelled by its contract,
// proved in gate.rs: records become durable in the order written, a crash keeps any prefix
// of them (a checkpoint replaces every record before it), and a held send or a deferred
// effect happens only once every record written before it is durable. A ghost history
// records, over every incarnation, the slots whose top-priority request left for another
// recorder or reached this replica's register durably, and the reservations made durable.
//
// Proved, for every sequence of starts, requests, writes becoming durable, releases,
// checkpoints and crashes (slots and reservations below u64::MAX - 1):
// (a) a restarted replica never proposes at the top priority in a slot at or below a
//     reservation it made durable before the crash, nor in any slot where a top-priority
//     request of an earlier incarnation left it: those are all at or below the largest
//     reservation on disk, which never goes down (checkpoints included);
// (b) the phase goes `Recovering → Live → Fenced`, skipping any, and never back; it leaves
//     `Recovering` for `Live` only once a majority of the members at the decided prefix,
//     counting this replica, answered, or when there is nobody to ask; fenced, it stays so
//     with no probe timer;
// (c) catching up ends only once caught up: a due retry asks a peer exactly while the
//     replica is behind a peer or, outside the membership, below its read mark (the most
//     advanced peer, or the answerer with the highest mark); while behind a peer there is a
//     retry deadline, no later than now if a retry is due, at which one is due; a page from
//     the peer asked ends the request and asks again exactly while still behind, an empty
//     one leaves it to the timer, and a page from anyone else changes nothing.
//
// Departures from the original's code, forced by what vstd specifies, are marked DEV;
// VERIFICATION.md lists them. When recovery.rs, or those lines of reads.rs, proposing.rs,
// mod.rs and snapshot.rs, change, port the change here.
use vstd::prelude::*;

verus! {

/// `Timestamp::after`: saturating.
pub open spec fn after(t: u64, n: u64) -> u64 {
    if t as int + n as int > u64::MAX as int { u64::MAX } else { (t + n) as u64 }
}

// DEV: `saturating_add` spelled out (no vstd spec).
fn after_exec(t: u64, n: u64) -> (r: u64)
    ensures r == after(t, n),
{
    if t > u64::MAX - n { u64::MAX } else { t + n }
}

// DEV: `<[_]>::contains` as a scan.
fn contains(v: &Vec<u64>, x: u64) -> (r: bool)
    ensures r == v@.contains(x),
{
    let mut i = 0;
    while i < v.len()
        invariant i <= v.len(), forall|j: int| 0 <= j < i ==> v@[j] != x,
        decreases v.len() - i,
    {
        if v[i] == x {
            assert(v@[i as int] == x);
            return true;
        }
        i += 1;
    }
    false
}

// ---- Phase ----

pub enum Phase {
    // DEV: a vector for the `BTreeSet` of answers (repeats are harmless: only membership
    // is read).
    Recovering { probe_at: u64, answered: Vec<u64> },
    Live,
    Fenced,
}

pub open spec fn rank(p: Phase) -> int {
    match p {
        Phase::Recovering { .. } => 0,
        Phase::Live => 1,
        Phase::Fenced => 2,
    }
}

pub open spec fn answers(p: Phase) -> Seq<u64> {
    match p {
        Phase::Recovering { answered, .. } => answered@,
        _ => Seq::empty(),
    }
}

pub open spec fn probe_spec(p: Phase) -> Option<u64> {
    match p {
        Phase::Recovering { probe_at, .. } => Some(probe_at),
        _ => None,
    }
}

/// Members of `members` that are `me` or answered.
pub open spec fn count(members: Seq<u64>, me: u64, answered: Seq<u64>) -> nat
    decreases members.len(),
{
    if members.len() == 0 {
        0
    } else {
        count(members.drop_last(), me, answered)
            + if members.last() == me || answered.contains(members.last()) { 1nat } else { 0nat }
    }
}

impl Phase {
    pub fn start(restarted: bool, now: u64) -> (p: Phase)
        ensures
            restarted ==> p is Recovering && probe_spec(p) == Some(now) && answers(p).len() == 0,
            !restarted ==> p is Live,
    {
        if restarted { Phase::Recovering { probe_at: now, answered: Vec::new() } } else { Phase::Live }
    }

    pub fn fenced(&self) -> (r: bool)
        ensures r == (*self is Fenced),
    {
        // DEV: `==` (derived `PartialEq`) as a match.
        match self {
            Phase::Fenced => true,
            _ => false,
        }
    }

    pub fn probe_at(&self) -> (r: Option<u64>)
        ensures r == probe_spec(*self),
    {
        match self {
            Phase::Recovering { probe_at, .. } => Some(*probe_at),
            Phase::Live | Phase::Fenced => None,
        }
    }

    pub fn probed(&mut self, at: Option<u64>)
        ensures
            rank(*final(self)) >= rank(*old(self)),
            !(*old(self) is Recovering) ==> *final(self) == *old(self),
            *old(self) is Recovering && at is None ==> *final(self) is Live,
            *old(self) is Recovering && at is Some ==> *final(self) is Recovering
                && probe_spec(*final(self)) == at && answers(*final(self)) == answers(*old(self)),
    {
        // DEV: `let .. else` and assignment through the binding, as a swap and a rebuild.
        if !(self.probe_at().is_some()) {
            return;
        }
        let mut taken = Phase::Live;
        std::mem::swap(self, &mut taken);
        if let Phase::Recovering { answered, .. } = taken {
            match at {
                Some(at) => *self = Phase::Recovering { probe_at: at, answered },
                None => {},
            }
        }
    }

    pub fn answered(&mut self, from: u64)
        ensures
            rank(*final(self)) == rank(*old(self)),
            probe_spec(*final(self)) == probe_spec(*old(self)),
            !(*old(self) is Recovering) ==> *final(self) == *old(self),
            *old(self) is Recovering ==> forall|x: u64|
                answers(*final(self)).contains(x) <==> (answers(*old(self)).contains(x) || x == from),
    {
        if !(self.probe_at().is_some()) {
            return;
        }
        // DEV: `BTreeSet::insert` through the binding, as a swap, a push and a rebuild.
        let mut taken = Phase::Live;
        std::mem::swap(self, &mut taken);
        if let Phase::Recovering { probe_at, mut answered } = taken {
            let ghost a0 = answered@;
            answered.push(from);
            assert forall|x: u64| #[trigger] answered@.contains(x) <==> (a0.contains(x) || x == from) by {
                if a0.contains(x) {
                    let j = choose|j: int| 0 <= j < a0.len() && a0[j] == x;
                    assert(answered@[j] == x);
                }
                if x == from {
                    assert(answered@[a0.len() as int] == x);
                }
                if answered@.contains(x) {
                    let j = choose|j: int| 0 <= j < answered@.len() && answered@[j] == x;
                    if j < a0.len() {
                        assert(a0[j] == x);
                    }
                }
            }
            *self = Phase::Recovering { probe_at, answered };
        }
    }

    pub fn check(&mut self, me: u64, members: &Vec<u64>)
        ensures
            rank(*final(self)) >= rank(*old(self)),
            !(*old(self) is Recovering) ==> *final(self) == *old(self),
            *old(self) is Recovering ==> *final(self) is Recovering || *final(self) is Live,
            *old(self) is Recovering && *final(self) is Recovering ==> *final(self) == *old(self),
            *old(self) is Recovering && *final(self) is Live ==>
                count(members@, me, answers(*old(self))) > members@.len() / 2,
    {
        let n = match &*self {
            Phase::Recovering { answered, .. } => {
                // DEV: `filter(..).count()` as a loop.
                let mut n: usize = 0;
                let mut i: usize = 0;
                while i < members.len()
                    invariant
                        i <= members.len(), n <= i,
                        n as nat == count(members@.take(i as int), me, answered@),
                    decreases members.len() - i,
                {
                    assert(members@.take(i as int + 1).drop_last() =~= members@.take(i as int));
                    assert(members@.take(i as int + 1).last() == members@[i as int]);
                    if members[i] == me || contains(answered, members[i]) {
                        n += 1;
                    }
                    i += 1;
                }
                assert(members@.take(members.len() as int) =~= members@);
                n
            },
            _ => return,
        };
        if n > members.len() / 2 {
            *self = Phase::Live;
        }
    }

    pub fn fence(&mut self)
        ensures *final(self) is Fenced,
    {
        *self = Phase::Fenced;
    }
}

// ---- CatchUp ----

pub struct CatchUp {
    pub ahead: u64,
    pub peer: Option<u64>,
    pub request: Option<(u64, u64)>,
}

/// Outside the membership, with an answer to the round in flight past the prefix.
pub open spec fn below_mark(outside: bool, answers: Seq<(u64, u64)>, committed: u64) -> bool {
    outside && exists|i: int| 0 <= i < answers.len() && #[trigger] answers[i].1 > committed
}

/// `answers[i]` holds the highest mark.
pub open spec fn highest(answers: Seq<(u64, u64)>, i: int) -> bool {
    0 <= i < answers.len() && forall|j: int| 0 <= j < answers.len() ==> #[trigger] answers[j].1 <= answers[i].1
}

/// `Replica::read_catch_up_from`: `outside` is `Replica::outside`, `round` the marks answered
/// in the round in flight, if any.
// DEV: the round's `BTreeMap` of answers as a vector of (replica, mark).
pub fn read_catch_up_from(outside: bool, round: Option<&Vec<(u64, u64)>>, committed: u64) -> (r: Option<u64>)
    ensures
        r is Some <==> round is Some && below_mark(outside, round.unwrap()@, committed),
        r matches Some(p) ==> exists|i: int| #[trigger] highest(round.unwrap()@, i) && round.unwrap()@[i].0 == p,
{
    // DEV: `filter(..)?` as early returns.
    if !outside {
        return None;
    }
    let answers = match round {
        Some(a) => a,
        None => return None,
    };
    // DEV: `max_by_key` (the last of equal maxima) as a loop.
    let mut best: Option<usize> = None;
    let mut i: usize = 0;
    while i < answers.len()
        invariant
            i <= answers.len(),
            best is None <==> i == 0,
            best matches Some(b) ==> b < i
                && forall|j: int| 0 <= j < i ==> #[trigger] answers@[j].1 <= answers@[b as int].1,
        decreases answers.len() - i,
    {
        let keep = match best {
            Some(b) => answers[i].1 < answers[b].1,
            None => false,
        };
        if !keep {
            best = Some(i);
        }
        i += 1;
    }
    let b = match best {
        Some(b) => b,
        None => return None,
    };
    let (peer, mark) = answers[b];
    proof {
        assert(highest(answers@, b as int));
        if committed < mark {
            assert(answers@[b as int].1 > committed);
        }
    }
    if committed < mark { Some(peer) } else { None }
}

impl CatchUp {
    pub open spec fn inv(self) -> bool {
        self.ahead > 0 ==> self.peer is Some
    }

    pub open spec fn behind_spec(self, committed: u64) -> bool {
        self.ahead as int > committed as int + 1
    }

    pub open spec fn due_spec(self, now: u64, interval: u64) -> bool {
        match self.request {
            None => true,
            Some((_, t)) => after(t, interval) <= now,
        }
    }

    pub fn new() -> (c: CatchUp)
        ensures c.inv(), c.request is None, c.ahead == 0,
    {
        CatchUp { ahead: 0, peer: None, request: None }
    }

    pub fn heard(&mut self, from: u64, commit_index: u64)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).request == old(self).request,
            final(self).ahead >= old(self).ahead, final(self).ahead >= commit_index,
    {
        if commit_index > self.ahead {
            self.ahead = commit_index;
            self.peer = Some(from);
        }
    }

    pub fn ahead_of(&self, committed: u64) -> (r: bool)
        ensures r == (self.ahead > committed),
    {
        self.ahead > committed
    }

    // DEV: `committed + 1` widened (the original's never overflows: slots are below MAX_SLOT).
    pub fn behind(&self, committed: u64) -> (r: bool)
        ensures r == self.behind_spec(committed),
    {
        self.ahead as u128 > committed as u128 + 1
    }

    pub fn due(&self, now: u64, interval: u64) -> (r: bool)
        ensures r == self.due_spec(now, interval),
    {
        // DEV: `is_none_or` as a match.
        match self.request {
            None => true,
            Some((_, t)) => after_exec(t, interval) <= now,
        }
    }

    pub fn requested(&mut self, peer: u64, now: u64)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).request == Some((peer, now)),
            final(self).ahead == old(self).ahead, final(self).peer == old(self).peer,
    {
        self.request = Some((peer, now));
    }

    pub fn page(&mut self, from: u64, empty: bool, committed: u64, now: u64) -> (r: Option<u64>)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).ahead == old(self).ahead, final(self).peer == old(self).peer,
            !(old(self).request matches Some((p, _)) && p == from) ==>
                r is None && final(self).request == old(self).request,
            (old(self).request matches Some((p, _)) && p == from) && empty ==>
                r is None && final(self).request == Some((from, now)),
            (old(self).request matches Some((p, _)) && p == from) && !empty ==>
                final(self).request is None && (r is Some <==> old(self).behind_spec(committed))
                    && (r matches Some(q) ==> q == from),
    {
        // DEV: `is_some_and` as a match.
        let asked = match self.request {
            Some((p, _)) => p == from,
            None => false,
        };
        if !asked {
            return None;
        }
        if empty {
            self.request = Some((from, now));
            return None;
        }
        self.request = None;
        if self.behind(committed) { Some(from) } else { None }
    }

    pub fn installed(&mut self, committed: u64) -> (r: Option<u64>)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).request is None, final(self).ahead == old(self).ahead,
            r is Some <==> old(self).ahead > committed,
            r matches Some(p) ==> old(self).peer == Some(p),
    {
        self.request = None;
        // DEV: `filter` as a branch.
        if self.ahead_of(committed) { self.peer } else { None }
    }

    pub fn deadline(&self, committed: u64, now: u64, interval: u64) -> (r: Option<u64>)
        requires self.inv(),
        ensures
            r is Some <==> self.behind_spec(committed),
            r matches Some(t) ==> self.due_spec(t, interval),
            r matches Some(t) ==> (self.due_spec(now, interval) ==> t <= now),
    {
        // DEV: `or(map(..)).is_some()` and `map_or` as matches.
        let known = self.request.is_some() || self.peer.is_some();
        let behind = self.behind(committed) && known;
        if !behind {
            return None;
        }
        match self.request {
            None => Some(now),
            Some((_, t)) => Some(after_exec(t, interval)),
        }
    }

    // DEV: the closure `read_from` passed evaluated (it has no effects).
    pub fn retry(&self, committed: u64, now: u64, interval: u64, read_from: Option<u64>) -> (r: Option<u64>)
        requires self.inv(),
        ensures
            !self.due_spec(now, interval) ==> r is None,
            self.due_spec(now, interval) && self.behind_spec(committed) ==> r == self.peer && r is Some,
            self.due_spec(now, interval) && !self.behind_spec(committed) ==> r == read_from,
    {
        if !self.due(now, interval) {
            return None;
        }
        if self.behind(committed) {
            // DEV: `or(map(..))` as matches.
            match self.peer {
                Some(p) => Some(p),
                None => match self.request {
                    Some((p, _)) => Some(p),
                    None => None,
                },
            }
        } else {
            read_from
        }
    }
}

/// `fire_timers`' catch-up, with the read mark of the round in flight (`round`).
pub fn fire_catch_up(
    c: &CatchUp,
    committed: u64,
    now: u64,
    interval: u64,
    outside: bool,
    round: Option<&Vec<(u64, u64)>>,
) -> (r: Option<u64>)
    requires c.inv(),
    ensures
        !c.due_spec(now, interval) ==> r is None,
        // (c): a due retry asks exactly while behind a peer or below the read mark.
        c.due_spec(now, interval) ==> (r is Some <==> c.behind_spec(committed)
            || (round is Some && below_mark(outside, round.unwrap()@, committed))),
        r matches Some(p) ==> (c.behind_spec(committed) && c.peer == Some(p))
            || exists|i: int| #[trigger] highest(round.unwrap()@, i) && round.unwrap()@[i].0 == p,
{
    let read_from = read_catch_up_from(outside, round, committed);
    c.retry(committed, now, interval, read_from)
}

// ---- Reservation ----

pub struct Reservation {
    pub floor: u64,
    pub through: Option<u64>,
    pub durable: Option<u64>,
}

pub open spec fn val(o: Option<u64>) -> int {
    match o {
        Some(t) => t as int,
        None => -1,
    }
}

impl Reservation {
    /// The latest reservation made, by this incarnation or recovered; -1 for none.
    pub open spec fn cover(self) -> int {
        match self.through {
            Some(t) => t as int,
            None => self.floor as int - 1,
        }
    }

    // DEV: the recovered reservation is below u64::MAX (the replica's own writes are).
    pub fn recovered(through: Option<u64>) -> (r: Reservation)
        requires val(through) < u64::MAX,
        ensures r.floor as int == val(through) + 1, r.through is None, r.durable is None,
    {
        // DEV: `map_or` as a match.
        let floor = match through {
            Some(r) => r + 1,
            None => 0,
        };
        Reservation { floor, through: None, durable: None }
    }

    pub fn may_lead(&self, slot: u64) -> (r: bool)
        ensures r == (slot >= self.floor),
    {
        slot >= self.floor
    }

    pub fn extend(&mut self, slot: u64, span: u64) -> (r: Option<u64>)
        requires slot as int + span as int <= u64::MAX,
        ensures
            final(self).floor == old(self).floor && final(self).durable == old(self).durable,
            r is None ==> final(self).through == old(self).through
                && (old(self).through matches Some(t) && slot <= t),
            r matches Some(t) ==> t == slot + span && final(self).through == Some(t)
                && (old(self).through matches Some(o) ==> slot > o),
    {
        // DEV: `is_some_and` as a match.
        if let Some(t) = self.through {
            if slot <= t {
                return None;
            }
        }
        let through = slot + span;
        self.through = Some(through);
        Some(through)
    }

    pub fn made_durable(&mut self, through: u64)
        ensures
            final(self).floor == old(self).floor && final(self).through == old(self).through,
            val(final(self).durable) == if val(old(self).durable) >= through as int {
                val(old(self).durable)
            } else {
                through as int
            },
    {
        // DEV: `Option::max` as a match.
        self.durable = match self.durable {
            Some(d) => if d >= through { Some(d) } else { Some(through) },
            None => Some(through),
        };
    }

    pub fn covers(&self, slot: u64) -> (r: bool)
        ensures r == (slot as int <= val(self.durable)),
    {
        // DEV: `is_some_and` as a match.
        match self.durable {
            Some(t) => slot <= t,
            None => false,
        }
    }

    pub fn checkpoint(&self) -> (r: Option<u64>)
        requires self.floor as int - 1 <= self.cover(),
        ensures val(r) == self.cover(),
    {
        // DEV: `or(checked_sub(1))` as matches.
        match self.through {
            Some(t) => Some(t),
            None => if self.floor > 0 { Some(self.floor - 1) } else { None },
        }
    }
}

/// A record written: a reservation, a checkpoint (replacing every record before it; it
/// carries the reservation `checkpoint` returns, if any), or this replica's register
/// recording its own top-priority value in a slot.
pub enum Op {
    Reserve(u64),
    Replace(Option<u64>),
    Record(u64),
}

/// The largest reservation on disk, `d` before `op` is durable.
pub open spec fn step(d: int, op: Op) -> int {
    match op {
        Op::Reserve(t) => if t as int > d { t as int } else { d },
        Op::Replace(o) => val(o),
        Op::Record(_) => d,
    }
}

pub open spec fn fold(d: int, ops: Seq<Op>) -> int
    decreases ops.len(),
{
    if ops.len() == 0 { d } else { fold(step(d, ops[0]), ops.subrange(1, ops.len() as int)) }
}

pub proof fn lemma_fold_push(d: int, ops: Seq<Op>, op: Op)
    ensures fold(d, ops.push(op)) == step(fold(d, ops), op),
    decreases ops.len(),
{
    let pushed = ops.push(op);
    assert(pushed[0] == if ops.len() == 0 { op } else { ops[0] });
    if ops.len() == 0 {
        assert(pushed.subrange(1, 1) =~= Seq::<Op>::empty());
        assert(fold(step(d, op), Seq::<Op>::empty()) == step(d, op));
    } else {
        let rest = ops.subrange(1, ops.len() as int);
        assert(pushed.subrange(1, pushed.len() as int) =~= rest.push(op));
        lemma_fold_push(step(d, ops[0]), rest, op);
        assert(fold(d, ops) == fold(step(d, ops[0]), rest));
    }
}

/// A prefix of `k` records, once the first is durable.
pub proof fn lemma_fold_pop(d: int, ops: Seq<Op>, k: int)
    requires 1 <= k <= ops.len(),
    ensures fold(d, ops.take(k)) == fold(step(d, ops[0]), ops.subrange(1, ops.len() as int).take(k - 1)),
{
    assert(ops.take(k).subrange(1, k) =~= ops.subrange(1, ops.len() as int).take(k - 1));
}

/// What waits for every record written before it: the reservation becoming durable here
/// (`Effect::ReservationDurable`), or a held top-priority request.
pub enum Wait {
    Durable(u64),
    Send(u64),
}

pub open spec fn wait_val(w: Wait) -> int {
    match w {
        Wait::Durable(t) => t as int,
        Wait::Send(s) => s as int,
    }
}

/// A replica's reservation and what the gate holds for it, across crashes.
pub struct Host {
    pub res: Reservation,
    /// Waiting work, in the order the gate releases it.
    pub waits: Vec<Wait>,
    /// For each, how many records of the incarnation must be durable first.
    pub tags: Ghost<Seq<int>>,
    /// Records written and not yet durable, in order.
    pub ops: Ghost<Seq<Op>>,
    /// Records of this incarnation durable so far.
    pub done: Ghost<int>,
    /// The largest reservation on disk, -1 for none.
    pub disk: Ghost<int>,
    /// Slots whose top-priority request left for another recorder or is durable in this
    /// replica's register, in any incarnation.
    pub escaped: Ghost<Set<u64>>,
    /// Reservations ever durable, in any incarnation.
    pub reserved: Ghost<Set<int>>,
    /// `escaped` and `reserved` as of the last crash.
    pub escaped_before: Ghost<Set<u64>>,
    pub reserved_before: Ghost<Set<int>>,
    /// Slots this incarnation proposed at the top priority.
    pub top: Ghost<Set<u64>>,
}

impl Host {
    /// The largest reservation on disk once `n` records of the incarnation are durable.
    pub open spec fn reach(self, n: int) -> int {
        if n <= self.done@ { self.disk@ } else { fold(self.disk@, self.ops@.take(n - self.done@)) }
    }

    pub open spec fn inv(self) -> bool {
        &&& self.res.through matches Some(t) ==> t >= self.res.floor
        &&& self.res.cover() < u64::MAX - 1
        &&& self.res.floor as int - 1 <= self.res.cover()
        &&& self.done@ >= 0
        &&& fold(self.disk@, self.ops@) == self.res.cover()
        // The disk's reservation never goes down as records become durable.
        &&& forall|k: int| 0 <= k < self.ops@.len() ==>
                step(fold(self.disk@, #[trigger] self.ops@.take(k)), self.ops@[k]) >= fold(self.disk@, self.ops@.take(k))
        // This replica's register records a top-priority value only after its reservation.
        &&& forall|k: int| 0 <= k < self.ops@.len() ==> (#[trigger] self.ops@[k] matches Op::Record(s)
                ==> s as int <= fold(self.disk@, self.ops@.take(k)))
        &&& self.disk@ >= self.res.floor as int - 1
        &&& val(self.res.durable) <= self.disk@
        &&& self.waits@.len() == self.tags@.len()
        &&& forall|i: int| 0 <= i < self.waits@.len() ==> {
                &&& #[trigger] self.tags@[i] <= self.done@ + self.ops@.len()
                &&& wait_val(self.waits@[i]) <= self.reach(self.tags@[i])
            }
        &&& forall|e: u64| #[trigger] self.escaped@.contains(e) ==> e as int <= self.disk@
        &&& forall|r: int| #[trigger] self.reserved@.contains(r) ==> r <= self.disk@
        &&& forall|e: u64| #[trigger] self.escaped_before@.contains(e) ==> e < self.res.floor
        &&& forall|r: int| #[trigger] self.reserved_before@.contains(r) ==> r < self.res.floor
        &&& forall|s: u64| #[trigger] self.top@.contains(s) ==> self.res.floor <= s && s as int <= self.res.cover()
    }

    pub proof fn lemma_prefix(self, k: int)
        requires self.inv(), 0 <= k <= self.ops@.len(),
        ensures fold(self.disk@, self.ops@.take(k)) >= self.disk@,
        decreases k,
    {
        if k == 0 {
            assert(self.ops@.take(0) =~= Seq::<Op>::empty());
        } else {
            self.lemma_prefix(k - 1);
            assert(self.ops@.take(k) =~= self.ops@.take(k - 1).push(self.ops@[k - 1]));
            lemma_fold_push(self.disk@, self.ops@.take(k - 1), self.ops@[k - 1]);
        }
    }

    /// A fresh replica: nothing on disk.
    pub fn fresh() -> (h: Host)
        ensures h.inv(), h.escaped_before@.is_empty(), h.reserved_before@.is_empty(),
    {
        Host {
            res: Reservation::recovered(None),
            waits: Vec::new(),
            tags: Ghost(Seq::empty()),
            ops: Ghost(Seq::empty()),
            done: Ghost(0),
            disk: Ghost(-1),
            escaped: Ghost(Set::empty()),
            reserved: Ghost(Set::empty()),
            escaped_before: Ghost(Set::empty()),
            reserved_before: Ghost(Set::empty()),
            top: Ghost(Set::empty()),
        }
    }

    /// `Gate::write`: appending `op` keeps every prefix, so folds of the old records do not
    /// change.
    proof fn lemma_append(self, op: Op)
        requires self.inv(),
        ensures
            fold(self.disk@, self.ops@.push(op)) == step(self.res.cover(), op),
            self.ops@.push(op).take(self.ops@.len() as int) == self.ops@,
            forall|k: int| 0 <= k <= self.ops@.len() ==> #[trigger] self.ops@.push(op).take(k) == self.ops@.take(k),
    {
        lemma_fold_push(self.disk@, self.ops@, op);
        assert(self.ops@.push(op).take(self.ops@.len() as int) =~= self.ops@);
        assert forall|k: int| 0 <= k <= self.ops@.len() implies #[trigger] self.ops@.push(op).take(k) == self.ops@.take(k) by {
            assert(self.ops@.push(op).take(k) =~= self.ops@.take(k));
        }
    }

    /// `Replica::start`'s use of the reservation: whether this replica proposes `slot` at
    /// the top priority (`leads`: it leads the slot in the schedule; `span` is four windows).
    pub fn start(&mut self, slot: u64, leads: bool, span: u64) -> (leader: bool)
        requires old(self).inv(), (slot as int + span as int) < u64::MAX - 1,
        ensures
            final(self).inv(),
            final(self).escaped_before == old(self).escaped_before,
            final(self).reserved_before == old(self).reserved_before,
            leader ==> leads && final(self).top@.contains(slot),
            // (a): past every reservation durable before the crash, and every slot an
            // earlier incarnation sent at the top priority.
            leader ==> forall|r: int| #[trigger] old(self).reserved_before@.contains(r) ==> r < slot,
            leader ==> forall|e: u64| #[trigger] old(self).escaped_before@.contains(e) ==> e < slot,
    {
        let leader = leads && self.res.may_lead(slot);
        if leader {
            let ghost before = *self;
            if let Some(through) = self.res.extend(slot, span) {
                // `gate.write(Reservation)`, then `after_durable(ReservationDurable)`: with a
                // write pending, the effect waits for it.
                proof {
                    before.lemma_append(Op::Reserve(through));
                    self.ops = Ghost(self.ops@.push(Op::Reserve(through)));
                }
                self.waits.push(Wait::Durable(through));
                proof {
                    self.tags = Ghost(self.tags@.push(self.done@ + self.ops@.len()));
                    assert(self.ops@.take(self.ops@.len() as int) =~= self.ops@);
                    assert(self.tags@.last() == self.done@ + self.ops@.len());
                }
            }
            proof {
                self.top = Ghost(self.top@.insert(slot));
            }
        }
        leader
    }

    /// `send_requests` for one request in `slot` at `step` (4 is the first), `top` if it
    /// is this replica's own proposal at the top priority, `to_me` if to its own recorder,
    /// which records it.
    // DEV: a request at a later step leaves only after the replies to the first step, each
    // sent after this replica's records were durable, so the reservation is (precondition).
    pub fn send_request(&mut self, slot: u64, step: u32, top: bool, to_me: bool) -> (sent: bool)
        requires
            old(self).inv(), step >= 4,
            top ==> old(self).top@.contains(slot),
            top && step > 4 ==> slot as int <= val(old(self).res.durable),
        ensures
            final(self).inv(), final(self).res == old(self).res, final(self).top == old(self).top,
            final(self).escaped_before == old(self).escaped_before,
            final(self).reserved_before == old(self).reserved_before,
    {
        let h_ready = self.res.covers(slot);
        let gate = step == 4 && top && !h_ready && !to_me;
        if gate {
            // `gate.hold`.
            self.waits.push(Wait::Send(slot));
            proof {
                self.tags = Ghost(self.tags@.push(self.done@ + self.ops@.len()));
                assert(self.ops@.take(self.ops@.len() as int) =~= self.ops@);
            }
            return false;
        }
        if top {
            if to_me {
                proof {
                    self.lemma_append(Op::Record(slot));
                    self.ops = Ghost(self.ops@.push(Op::Record(slot)));
                }
            } else {
                proof {
                    self.escaped = Ghost(self.escaped@.insert(slot));
                }
            }
        }
        true
    }

    /// The oldest record written becomes durable.
    pub fn durable_one(&mut self)
        requires old(self).inv(), old(self).ops@.len() > 0,
        ensures
            final(self).inv(), final(self).res == old(self).res, final(self).top == old(self).top,
            final(self).disk@ >= old(self).disk@,
            final(self).escaped_before == old(self).escaped_before,
            final(self).reserved_before == old(self).reserved_before,
    {
        proof {
            let ops = self.ops@;
            let d = self.disk@;
            let op = ops[0];
            let d1 = step(d, op);
            let rest = ops.subrange(1, ops.len() as int);
            assert(ops.take(0) =~= Seq::<Op>::empty());
            assert forall|k: int| 0 <= k < rest.len() implies
                step(fold(d1, #[trigger] rest.take(k)), rest[k]) >= fold(d1, rest.take(k)) by {
                lemma_fold_pop(d, ops, k + 1);
                assert(rest[k] == ops[k + 1]);
            }
            assert forall|k: int| 0 <= k < rest.len() implies (#[trigger] rest[k] matches Op::Record(s)
                ==> s as int <= fold(d1, rest.take(k))) by {
                lemma_fold_pop(d, ops, k + 1);
                assert(rest[k] == ops[k + 1]);
            }
            let old_self = *self;
            self.disk = Ghost(d1);
            self.ops = Ghost(rest);
            self.done = Ghost(old_self.done@ + 1);
            match op {
                Op::Record(s) => {
                    self.escaped = Ghost(self.escaped@.insert(s));
                },
                Op::Reserve(t) => {
                    self.reserved = Ghost(self.reserved@.insert(t as int));
                },
                Op::Replace(o) => {
                    if o is Some {
                        self.reserved = Ghost(self.reserved@.insert(val(o)));
                    }
                },
            }
            assert forall|i: int| 0 <= i < self.waits@.len() implies {
                &&& #[trigger] self.tags@[i] <= self.done@ + self.ops@.len()
                &&& wait_val(self.waits@[i]) <= self.reach(self.tags@[i])
            } by {
                let n = self.tags@[i];
                assert(wait_val(old_self.waits@[i]) <= old_self.reach(n));
                if n > old_self.done@ + 1 {
                    lemma_fold_pop(d, ops, n - old_self.done@);
                } else if n == old_self.done@ + 1 {
                    lemma_fold_pop(d, ops, 1);
                    assert(rest.take(0) =~= Seq::<Op>::empty());
                }
            }
        }
    }

    /// The gate releases the oldest waiting work, everything written before it durable.
    pub fn release(&mut self)
        requires old(self).inv(), old(self).waits@.len() > 0, old(self).tags@[0] <= old(self).done@,
        ensures
            final(self).inv(), final(self).top == old(self).top,
            final(self).escaped_before == old(self).escaped_before,
            final(self).reserved_before == old(self).reserved_before,
            final(self).res.floor == old(self).res.floor && final(self).res.through == old(self).res.through,
    {
        let ghost w0 = self.waits@;
        let ghost t0 = self.tags@;
        let w = self.waits.remove(0);
        proof {
            assert(wait_val(w) <= self.disk@);
            self.tags = Ghost(t0.subrange(1, t0.len() as int));
            assert forall|i: int| 0 <= i < self.waits@.len() implies {
                &&& #[trigger] self.tags@[i] <= self.done@ + self.ops@.len()
                &&& wait_val(self.waits@[i]) <= self.reach(self.tags@[i])
            } by {
                assert(self.waits@[i] == w0[i + 1]);
                assert(self.tags@[i] == t0[i + 1]);
            }
        }
        match w {
            // `Effect::ReservationDurable` (`Replica::apply`).
            Wait::Durable(t) => self.res.made_durable(t),
            Wait::Send(s) => {
                proof {
                    self.escaped = Ghost(self.escaped@.insert(s));
                }
            },
        }
    }

    /// `Replica::checkpoint`'s reservation record: the next write replaces every record.
    pub fn checkpoint(&mut self)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).res == old(self).res, final(self).top == old(self).top,
            final(self).escaped_before == old(self).escaped_before,
            final(self).reserved_before == old(self).reserved_before,
    {
        let r = self.res.checkpoint();
        proof {
            self.lemma_append(Op::Replace(r));
            self.ops = Ghost(self.ops@.push(Op::Replace(r)));
        }
    }

    /// A crash, after which the records still in flight are lost, and the restart from
    /// what is on disk: `Replica::new` recovers the largest reservation, `recovered`.
    pub fn restart(&mut self, recovered: Option<u64>)
        requires old(self).inv(), val(recovered) == old(self).disk@,
        ensures
            final(self).inv(),
            final(self).escaped_before == old(self).escaped@,
            final(self).reserved_before == old(self).reserved@,
            final(self).res.floor as int == old(self).disk@ + 1,
    {
        proof {
            self.lemma_prefix(self.ops@.len() as int);
            assert(self.ops@.take(self.ops@.len() as int) =~= self.ops@);
        }
        let res = Reservation::recovered(recovered);
        self.res = res;
        self.waits = Vec::new();
        proof {
            self.tags = Ghost(Seq::empty());
            self.ops = Ghost(Seq::empty());
            self.done = Ghost(0);
            self.escaped_before = self.escaped;
            self.reserved_before = self.reserved;
            self.top = Ghost(Set::empty());
        }
    }
}

} // verus!
