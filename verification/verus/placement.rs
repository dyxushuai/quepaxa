// crates/quepaxa/src/placement_policy.rs, ported to Verus: leader placement's state machine
// (verification/verus/prove.sh checks it).
//
// Every transition (`new`, `on_order_applied`, `on_restart`, `sample`, `on_submitted`,
// `on_resolved`, `decide`) keeps `inv` and states what it changes, so the properties below
// hold for every sequence of them. Proved:
// (a) a member `decide` returns is never excluded at `now`; one it explores or exploits is
//     not demoted (excluded within four holds).
// (b) a measurement counts (`measured_spec`) only if its samples span the sustained period
//     and are not stale, for the leader and its candidates alike; the leader's own samples
//     all come from its current tenure, two rounds after it started (`inv`).
// (c) exploring returns only a tied member not measured; one measured is returned only once
//     every tied member is, outside a hold. A member's measurement changes only while it
//     leads (`led_kept`), so a member measured stays measured, and is not tried, until its
//     samples go stale: no endless trials.
// (d) `Pending` holds a target or a move, never both (by type); a move stays pending through
//     every transition, `decide` proposes nothing meanwhile, and it returns to `Idle` exactly
//     when `on_resolved` gets its id.
// (e) `new` and `on_restart` start the tenure at the given `now`, with no samples.
// Assumed: `order` and `costs` have one entry per member, the leader first (ranking.rs).
//
// Nodes are u32, timestamps and proposal ids u64. Departures from the original's code,
// forced by what vstd specifies, are marked DEV; VERIFICATION.md lists them. When
// placement_policy.rs changes, port the change here. Test-only accessors are not ported.
use vstd::prelude::*;
use std::collections::BTreeMap;

verus! {

broadcast use {vstd::std_specs::btree::group_btree_axioms, vstd::laws_cmp::group_laws_cmp};

pub const PROBE_INTERVAL: u64 = 1_000_000_000;
pub const MIN_ROUNDS: u64 = 10;
pub const STALE_HOLDS: u64 = 8;
pub const TIE: u128 = 11;
pub const TIE_SLACK: u128 = 5_000_000;
pub const WRITE_FLOOR: u64 = 8;

pub open spec fn sat64(x: int) -> int { if x > u64::MAX { u64::MAX as int } else { x } }
pub open spec fn sat128(x: int) -> int { if x > u128::MAX { u128::MAX as int } else { x } }
pub open spec fn sub0(a: int, b: int) -> int { if a >= b { a - b } else { 0 } }

#[derive(Clone, Copy)]
pub struct Tenure {
    pub leader: u32,
    pub since: u64,
}

#[derive(Clone, Copy)]
pub struct Target {
    pub member: u32,
    pub leader: u32,
    pub since: u64,
}

#[derive(Clone, Copy)]
pub enum Pending {
    Idle,
    Tracking(Target),
    Moving(u64),
}

#[derive(Clone, Copy)]
pub struct Exclusion {
    pub until: u64,
    pub factor: u64,
}

#[derive(Clone, Copy)]
pub struct Led {
    pub count: u64,
    pub sum: u128,
    pub sum_sq: u128,
    pub since: u64,
    pub at: u64,
}

/// The samples span the sustained period.
pub open spec fn spans_spec(l: Led, sustain: u64) -> bool {
    l.count >= MIN_ROUNDS && sat64(sub0(l.at as int, l.since as int) + 2 * PROBE_INTERVAL) >= sustain
}

/// The one rule for the leader's own tenure and for the members it compares against.
pub open spec fn measured_spec(l: Led, now: u64, sustain: u64, stale: int) -> bool {
    spans_spec(l, sustain) && sub0(now as int, l.at as int) < stale
}

pub open spec fn measured_in(led: Map<u32, Led>, n: u32, now: u64, sustain: u64, stale: int) -> bool {
    led.contains_key(n) && measured_spec(led[n], now, sustain, stale)
}

pub open spec fn stale_of(hold: u64) -> int { sat64(hold * STALE_HOLDS) }

pub open spec fn excl(e: Map<u32, Exclusion>, n: u32, now: u64) -> bool {
    e.contains_key(n) && e[n].until > now
}

/// Excluded, or its exclusion ended less than four holds ago.
pub open spec fn demoted(e: Map<u32, Exclusion>, n: u32, now: u64, hold: u64) -> bool {
    e.contains_key(n) && sat64(e[n].until + sat64(hold * 4)) > now
}

pub open spec fn held(changed_at: Option<u64>, now: u64, hold: u64) -> bool {
    match changed_at {
        Some(t) => sub0(now as int, t as int) < hold,
        None => false,
    }
}

/// Member i costs about as much as the leader (index 0), and was not demoted.
pub open spec fn tied(e: Map<u32, Exclusion>, costs: Seq<Option<u128>>, order: Seq<u32>, now: u64,
    hold: u64, writes: u64, i: int) -> bool
{
    match costs[0] {
        Some(cur) => match costs[i] {
            Some(c) => sat128(c * 10) <= sat128(sat128(cur * TIE) + writes * TIE_SLACK * 10)
                && !demoted(e, order[i], now, hold),
            None => false,
        },
        None => false,
    }
}

/// Member i gains a fifth over the leader, by more than `slack`, and is not excluded.
pub open spec fn may_lead(e: Map<u32, Exclusion>, costs: Seq<Option<u128>>, order: Seq<u32>, now: u64,
    cur: u128, slack: u128, i: int) -> bool
{
    match costs[i] {
        Some(c) => sat128(c * 5) <= sat128(cur * 4) && sub0(cur as int, c as int) > slack
            && !excl(e, order[i], now),
        None => false,
    }
}

/// Measurements of members other than `leader` were kept or dropped, never changed or added.
pub open spec fn led_kept(a: Map<u32, Led>, b: Map<u32, Led>, leader: u32) -> bool {
    forall|n: u32| n != leader && #[trigger] b.contains_key(n) ==> a.contains_key(n) && b[n] == a[n]
}

/// Exclusions were kept or dropped, never changed or added.
pub open spec fn excl_kept(a: Map<u32, Exclusion>, b: Map<u32, Exclusion>) -> bool {
    forall|n: u32| #[trigger] b.contains_key(n) ==> a.contains_key(n) && b[n] == a[n]
}

impl Led {
    /// DEV: `Led::default()`.
    pub fn empty() -> (l: Led)
        ensures l.count == 0,
    {
        Led { count: 0, sum: 0, sum_sq: 0, since: 0, at: 0 }
    }

    pub fn add(&mut self, us: u64, at: u64)
        ensures
            final(self).at == at, final(self).count >= 1,
            old(self).count == 0 ==> final(self).since == at,
            old(self).count != 0 ==> final(self).since == old(self).since,
    {
        if self.count == 0 {
            self.since = at;
        }
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(us as u128);
        proof {
            let x = us as int;
            assert(x * x <= 0xffff_ffff_ffff_ffff * 0xffff_ffff_ffff_ffff) by (nonlinear_arith)
                requires 0 <= x <= 0xffff_ffff_ffff_ffff;
        }
        self.sum_sq = self.sum_sq.saturating_add((us as u128) * (us as u128));
        self.at = at;
    }

    /// DEV: `count.max(1)` written out, here and in `var_of_mean`.
    pub fn mean(&self) -> u128 {
        self.sum / (if self.count == 0 { 1 } else { self.count }) as u128
    }

    fn var_of_mean(&self) -> u128 {
        let n = (if self.count == 0 { 1 } else { self.count }) as u128;
        self.sum_sq.saturating_sub(self.sum.saturating_mul(self.sum) / n) / n / n
    }

    fn worse_than(&self, other: &Led) -> bool {
        let (a, b) = (self.mean(), other.mean());
        a > b && (a - b).saturating_mul(a - b) > 9u128.saturating_mul(self.var_of_mean().saturating_add(other.var_of_mean()))
    }

    pub fn spans(&self, sustain: u64) -> (r: bool)
        ensures r == spans_spec(*self, sustain),
    {
        self.count >= MIN_ROUNDS && self.at.saturating_sub(self.since).saturating_add(2 * PROBE_INTERVAL) >= sustain
    }

    pub fn measured(&self, now: u64, sustain: u64, stale: u64) -> (r: bool)
        ensures r == measured_spec(*self, now, sustain, stale as int),
    {
        self.spans(sustain) && now.saturating_sub(self.at) < stale
    }
}

/// The entries of `m` whose keys are in `members`. DEV: for `retain(|n, _| members.contains(n))`.
fn keep<V: Copy>(m: &BTreeMap<u32, V>, members: &Vec<u32>) -> (r: BTreeMap<u32, V>)
    ensures
        forall|k: u32| #[trigger] r@.contains_key(k) == (m@.contains_key(k) && members@.contains(k)),
        forall|k: u32| #[trigger] r@.contains_key(k) ==> r@[k] == m@[k],
{
    let mut r: BTreeMap<u32, V> = BTreeMap::new();
    let mut i: usize = 0;
    while i < members.len()
        invariant
            i <= members@.len(),
            forall|k: u32| #[trigger] r@.contains_key(k)
                == (m@.contains_key(k) && exists|j: int| 0 <= j < i && members@[j] == k),
            forall|k: u32| #[trigger] r@.contains_key(k) ==> r@[k] == m@[k],
        decreases members@.len() - i,
    {
        let k = members[i];
        let ghost r0 = r@;
        match m.get(&k) {
            Some(v) => { r.insert(k, *v); }
            None => {}
        }
        proof {
            assert forall|x: u32| #[trigger] r@.contains_key(x)
                == (m@.contains_key(x) && exists|j: int| 0 <= j < i + 1 && members@[j] == x) by {
                if x == k {
                    assert(members@[i as int] == x);
                } else if exists|j: int| 0 <= j < i + 1 && members@[j] == x {
                    let j = choose|j: int| 0 <= j < i + 1 && members@[j] == x;
                    assert(j < i);
                }
            }
        }
        i = i + 1;
    }
    r
}

pub struct Policy {
    pub tenure: Tenure,
    pub pending: Pending,
    pub last_round: Option<u64>,
    pub changed_at: Option<u64>,
    pub excluded: BTreeMap<u32, Exclusion>,
    pub led: BTreeMap<u32, Led>,
}

impl Policy {
    /// The leader in force's samples are all from its current tenure, from two rounds in.
    pub open spec fn inv(&self) -> bool {
        self.led@.contains_key(self.tenure.leader)
            ==> self.led@[self.tenure.leader].since >= self.tenure.since + 2 * PROBE_INTERVAL
    }

    pub open spec fn same_but_pending(&self, o: &Policy) -> bool {
        &&& self.tenure == o.tenure
        &&& self.last_round == o.last_round
        &&& self.changed_at == o.changed_at
        &&& self.excluded@ == o.excluded@
        &&& self.led@ == o.led@
    }

    /// What exploring or exploiting may return.
    pub open spec fn explore_ok(&self, now: u64, sustain: u64, hold: u64, writes: u64, order: Seq<u32>,
        costs: Seq<Option<u128>>, m: u32) -> bool
    {
        &&& !demoted(self.excluded@, m, now, hold)
        &&& self.tenure.leader == order[0] && sub0(now as int, self.tenure.since as int) >= sustain
        &&& measured_in(self.led@, order[0], now, sustain, stale_of(hold))
        &&& exists|i: int| #![trigger order[i]] 1 <= i < order.len() && order[i] == m
            && tied(self.excluded@, costs, order, now, hold, writes, i)
        &&& measured_in(self.led@, m, now, sustain, stale_of(hold)) ==> {
            &&& !held(self.changed_at, now, hold)
            &&& forall|i: int| 1 <= i < order.len() && #[trigger] tied(self.excluded@, costs, order, now, hold, writes, i)
                ==> measured_in(self.led@, order[i], now, sustain, stale_of(hold))
        }
    }

    pub fn new(now: u64, leader: u32) -> (p: Policy)
        ensures
            p.inv(), p.tenure == (Tenure { leader, since: now }), p.pending is Idle,
            p.led@ == Map::<u32, Led>::empty(), p.excluded@ == Map::<u32, Exclusion>::empty(),
            p.changed_at is None, p.last_round is None,
    {
        Policy {
            tenure: Tenure { leader, since: now },
            pending: Pending::Idle,
            last_round: None,
            changed_at: None,
            excluded: BTreeMap::new(),
            led: BTreeMap::new(),
        }
    }

    fn excluded_at(&self, member: u32, now: u64) -> (r: bool)
        ensures r == excl(self.excluded@, member, now),
    {
        // DEV: a match for `is_some_and`, here and below.
        match self.excluded.get(&member) {
            Some(e) => e.until > now,
            None => false,
        }
    }

    fn start_tenure(&mut self, leader: u32, now: u64)
        ensures
            final(self).tenure == (Tenure { leader, since: now }),
            final(self).led@ == old(self).led@.remove(leader),
            final(self).pending == old(self).pending, final(self).last_round == old(self).last_round,
            final(self).changed_at == old(self).changed_at, final(self).excluded@ == old(self).excluded@,
    {
        self.tenure = Tenure { leader, since: now };
        self.led.remove(&leader);
    }

    fn untrack(&mut self)
        ensures
            final(self).same_but_pending(old(self)),
            old(self).pending is Tracking ==> final(self).pending is Idle,
            !(old(self).pending is Tracking) ==> final(self).pending == old(self).pending,
    {
        if let Pending::Tracking(_) = self.pending {
            self.pending = Pending::Idle;
        }
    }

    fn held_at(&self, now: u64, hold: u64) -> (r: bool)
        ensures r == held(self.changed_at, now, hold),
    {
        match self.changed_at {
            Some(t) => now.saturating_sub(t) < hold,
            None => false,
        }
    }

    /// DEV: `prev` and `next` for `old` and `new` (`old` is Verus syntax).
    pub fn on_order_applied(&mut self, now: u64, hold: u64, prev: u32, next: u32, decided: bool)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).changed_at == Some(now),
            final(self).pending == old(self).pending, final(self).last_round == old(self).last_round,
            next == prev ==> final(self).tenure == old(self).tenure && final(self).led@ == old(self).led@
                && final(self).excluded@ == old(self).excluded@,
            next != prev ==> final(self).tenure == (Tenure { leader: next, since: now })
                && final(self).led@ == old(self).led@.remove(next),
            led_kept(old(self).led@, final(self).led@, final(self).tenure.leader),
            decided ==> final(self).excluded@ == old(self).excluded@,
            !decided && next != prev ==> final(self).excluded@.contains_key(prev)
                && final(self).excluded@[prev].until >= now
                && final(self).excluded@.remove(prev) == old(self).excluded@.remove(prev),
    {
        self.changed_at = Some(now);
        if next == prev {
            return;
        }
        self.start_tenure(next, now);
        if !decided {
            // DEV: a match for `map_or`.
            let factor = match self.excluded.get(&prev) {
                Some(e) => e.factor.saturating_mul(2),
                None => 1,
            };
            let until = now.saturating_add(hold.saturating_mul(factor));
            self.excluded.insert(prev, Exclusion { until, factor });
            assert(self.excluded@.remove(prev) =~= old(self).excluded@.remove(prev));
        }
    }

    pub fn on_restart(&mut self, now: u64, leader: u32, changed: bool)
        requires old(self).inv(),
        ensures
            final(self).inv(),
            final(self).tenure == (Tenure { leader, since: now }),
            final(self).led@ == old(self).led@.remove(leader),
            final(self).excluded@ == old(self).excluded@, final(self).last_round == old(self).last_round,
            changed ==> final(self).changed_at == Some(now) && !(final(self).pending is Tracking),
            !changed ==> final(self).changed_at == old(self).changed_at && final(self).pending == old(self).pending,
            old(self).pending is Moving ==> final(self).pending == old(self).pending,
    {
        self.start_tenure(leader, now);
        if changed {
            self.changed_at = Some(now);
            self.untrack();
        }
    }

    pub fn sample(&mut self, now: u64, hold: u64, members: &Vec<u32>, us: Option<u64>)
        requires old(self).inv(), members@.len() > 0,
        ensures
            final(self).inv(),
            final(self).tenure.leader == members@[0],
            old(self).tenure.leader == members@[0] ==> final(self).tenure == old(self).tenure,
            old(self).tenure.leader != members@[0] ==> final(self).tenure == (Tenure { leader: members@[0], since: now }),
            final(self).pending == old(self).pending, final(self).changed_at == old(self).changed_at,
            final(self).last_round == old(self).last_round,
            led_kept(old(self).led@, final(self).led@, final(self).tenure.leader),
            forall|n: u32| #[trigger] final(self).led@.contains_key(n) ==> members@.contains(n),
            excl_kept(old(self).excluded@, final(self).excluded@),
    {
        let leader = members[0];
        if leader != self.tenure.leader {
            self.start_tenure(leader, now);
        }
        let ghost led0 = self.led@;
        let ghost ex0 = self.excluded@;
        if now.saturating_sub(self.tenure.since) >= hold {
            self.excluded.remove(&leader);
        }
        let ghost ex1 = self.excluded@;
        self.excluded = keep(&self.excluded, members);
        self.led = keep(&self.led, members);
        assert(excl_kept(ex0, ex1));
        assert(excl_kept(ex1, self.excluded@));
        assert(excl_kept(old(self).excluded@, ex0));
        assert(members@[0] == leader);
        if now.saturating_sub(self.tenure.since) < 2 * PROBE_INTERVAL {
            return;
        }
        match us {
            Some(us) => {
                // DEV: get, add, insert for `entry(leader).or_default().add(..)`.
                let mut l = match self.led.get(&leader) {
                    Some(l) => *l,
                    None => Led::empty(),
                };
                l.add(us, now);
                self.led.insert(leader, l);
            }
            None => {}
        }
    }

    pub fn on_submitted(&mut self, id: u64)
        requires !(old(self).pending is Moving),
        ensures final(self).same_but_pending(old(self)), final(self).pending == Pending::Moving(id),
    {
        // DEV: the `debug_assert!` that no move is pending is the precondition.
        self.pending = Pending::Moving(id);
    }

    pub fn on_resolved(&mut self, id: u64) -> (r: bool)
        ensures
            final(self).same_but_pending(old(self)),
            r == (old(self).pending == Pending::Moving(id)),
            r ==> final(self).pending is Idle,
            !r ==> final(self).pending == old(self).pending,
    {
        // DEV: a match for `!= Pending::Moving(id)`.
        match self.pending {
            Pending::Moving(m) => {
                if m == id {
                    self.pending = Pending::Idle;
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    fn may_lead_at(&self, now: u64, current: u128, slack: u128, order: &Vec<u32>, costs: &Vec<Option<u128>>,
        i: usize) -> (r: bool)
        requires i < order@.len(), costs@.len() == order@.len(),
        ensures r == may_lead(self.excluded@, costs@, order@, now, current, slack, i as int),
    {
        match costs[i] {
            Some(c) => c.saturating_mul(5) <= current.saturating_mul(4) && current.saturating_sub(c) > slack
                && !self.excluded_at(order[i], now),
            None => false,
        }
    }

    /// DEV: `ranked: Option<(&[NodeId], &[Option<u128>])>` becomes a flag and the two
    /// vectors; `sustain` and `hold` are passed for `Periods`. The `min_by_key`s and
    /// `position` become index loops keeping the first minimum, as they do.
    pub fn decide(&mut self, now: u64, sustain: u64, hold: u64, writes: u64, ranked: bool,
        order: &Vec<u32>, costs: &Vec<Option<u128>>) -> (r: Option<u32>)
        requires old(self).inv(), order@.len() > 0, costs@.len() == order@.len(),
        ensures
            final(self).inv(),
            final(self).tenure == old(self).tenure, final(self).led@ == old(self).led@,
            final(self).excluded@ == old(self).excluded@, final(self).changed_at == old(self).changed_at,
            final(self).last_round == Some(now),
            old(self).pending is Moving ==> final(self).pending == old(self).pending && r is None,
            r matches Some(m) ==> ranked && writes >= WRITE_FLOOR && !excl(final(self).excluded@, m, now)
                && exists|i: int| 1 <= i < order@.len() && order@[i] == m,
            r matches Some(m) ==> (final(self).pending is Idle
                ==> final(self).explore_ok(now, sustain, hold, writes, order@, costs@, m)),
            r is Some && final(self).pending is Tracking ==> !held(final(self).changed_at, now, hold)
                && sub0(now as int, final(self).pending->Tracking_0.since as int) >= sustain,
    {
        let last = self.last_round;
        self.last_round = Some(now);
        let gap = match last {
            None => true,
            Some(t) => now.saturating_sub(t) > 2 * PROBE_INTERVAL,
        };
        if gap {
            self.untrack();
        }
        if !ranked || writes < WRITE_FLOOR {
            self.untrack();
            return None;
        }
        if let Pending::Moving(_) = self.pending {
            return None;
        }
        let current = match costs[0] {
            Some(c) => c,
            None => {
                self.untrack();
                return None;
            }
        };
        let leader = order[0];
        let slack = (writes as u128) * TIE_SLACK;
        let mut best: Option<usize> = None;
        let mut i: usize = 1;
        while i < order.len()
            invariant
                1 <= i <= order@.len(), costs@.len() == order@.len(),
                best matches Some(j) ==> 1 <= j < i && costs@[j as int] is Some
                    && may_lead(self.excluded@, costs@, order@, now, current, slack, j as int),
            decreases order@.len() - i,
        {
            if self.may_lead_at(now, current, slack, order, costs, i) {
                best = match best {
                    None => Some(i),
                    Some(j) => if costs[i].unwrap() < costs[j].unwrap() { Some(i) } else { Some(j) },
                };
            }
            i = i + 1;
        }
        let ghost mut tp: int = 0;
        let tracked_target: Option<Target> = match self.pending {
            Pending::Tracking(t) => {
                if t.leader == leader {
                    let mut p: usize = 0;
                    while p < order.len() && order[p] != t.member
                        invariant p <= order@.len(),
                        decreases order@.len() - p,
                    {
                        p = p + 1;
                    }
                    if p < order.len() && self.may_lead_at(now, current, slack, order, costs, p) {
                        proof { tp = p as int; }
                        Some(t)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        assert(tracked_target matches Some(t) ==> 0 <= tp < order@.len() && order@[tp] == t.member
            && may_lead(self.excluded@, costs@, order@, now, current, slack, tp));
        let target = match tracked_target {
            Some(t) => t,
            None => match best {
                Some(j) => Target { member: order[j], leader, since: now },
                None => {
                    self.pending = Pending::Idle;
                    return self.explore(now, sustain, hold, writes, order, costs);
                }
            },
        };
        self.pending = Pending::Tracking(target);
        if now.saturating_sub(target.since) < sustain || self.held_at(now, hold) {
            return None;
        }
        match best {
            Some(j) => Some(order[j]),
            None => Some(target.member),
        }
    }

    fn demoted_at(&self, n: u32, now: u64, hold: u64) -> (r: bool)
        ensures r == demoted(self.excluded@, n, now, hold),
    {
        match self.excluded.get(&n) {
            Some(e) => e.until.saturating_add(hold.saturating_mul(4)) > now,
            None => false,
        }
    }

    fn measured_at(&self, n: u32, now: u64, sustain: u64, stale: u64) -> (r: bool)
        ensures r == measured_in(self.led@, n, now, sustain, stale as int),
    {
        match self.led.get(&n) {
            Some(l) => l.measured(now, sustain, stale),
            None => false,
        }
    }

    fn tied_at(&self, now: u64, hold: u64, writes: u64, current: u128, order: &Vec<u32>,
        costs: &Vec<Option<u128>>, i: usize) -> (r: bool)
        requires 1 <= i < order@.len(), costs@.len() == order@.len(), costs@[0] == Some(current),
        ensures r == tied(self.excluded@, costs@, order@, now, hold, writes, i as int),
    {
        match costs[i] {
            Some(c) => c.saturating_mul(10) <= current.saturating_mul(TIE).saturating_add((writes as u128) * TIE_SLACK * 10)
                && !self.demoted_at(order[i], now, hold),
            None => false,
        }
    }

    /// DEV: the `tied` vector and the filters over it become two index loops.
    fn explore(&self, now: u64, sustain: u64, hold: u64, writes: u64, order: &Vec<u32>,
        costs: &Vec<Option<u128>>) -> (r: Option<u32>)
        requires order@.len() > 0, costs@.len() == order@.len(),
        ensures r matches Some(m) ==> self.explore_ok(now, sustain, hold, writes, order@, costs@, m),
    {
        let stale = hold.saturating_mul(STALE_HOLDS);
        assert(stale as int == stale_of(hold));
        let leader = order[0];
        let current = match costs[0] {
            Some(c) => c,
            None => return None,
        };
        if self.tenure.leader != leader || now.saturating_sub(self.tenure.since) < sustain {
            return None;
        }
        let mine = match self.led.get(&leader) {
            Some(l) => *l,
            None => return None,
        };
        if !mine.measured(now, sustain, stale) {
            return None;
        }
        // Exploring.
        let mut pick: Option<usize> = None;
        let mut i: usize = 1;
        while i < order.len()
            invariant
                1 <= i <= order@.len(), costs@.len() == order@.len(), costs@[0] == Some(current),
                pick matches Some(j) ==> 1 <= j < i && costs@[j as int] is Some
                    && tied(self.excluded@, costs@, order@, now, hold, writes, j as int)
                    && !measured_in(self.led@, order@[j as int], now, sustain, stale as int),
                pick is None ==> forall|k: int| 1 <= k < i
                    && #[trigger] tied(self.excluded@, costs@, order@, now, hold, writes, k)
                    ==> measured_in(self.led@, order@[k], now, sustain, stale as int),
            decreases order@.len() - i,
        {
            if self.tied_at(now, hold, writes, current, order, costs, i)
                && !self.measured_at(order[i], now, sustain, stale) {
                pick = match pick {
                    None => Some(i),
                    Some(j) => if costs[i].unwrap() < costs[j].unwrap() { Some(i) } else { Some(j) },
                };
            }
            i = i + 1;
        }
        if let Some(j) = pick {
            return Some(order[j]);
        }
        // Exploited.
        let mut best: Option<usize> = None;
        let mut bl = mine;
        let mut i: usize = 1;
        while i < order.len()
            invariant
                1 <= i <= order@.len(), costs@.len() == order@.len(), costs@[0] == Some(current),
                forall|k: int| 1 <= k < order@.len()
                    && #[trigger] tied(self.excluded@, costs@, order@, now, hold, writes, k)
                    ==> measured_in(self.led@, order@[k], now, sustain, stale as int),
                best matches Some(j) ==> 1 <= j < i
                    && tied(self.excluded@, costs@, order@, now, hold, writes, j as int),
            decreases order@.len() - i,
        {
            if self.tied_at(now, hold, writes, current, order, costs, i)
                && self.measured_at(order[i], now, sustain, stale) {
                if let Some(l) = self.led.get(&order[i]) {
                    let better = match best {
                        None => true,
                        Some(_) => l.mean() < bl.mean(),
                    };
                    if better {
                        best = Some(i);
                        bl = *l;
                    }
                }
            }
            i = i + 1;
        }
        let j = match best {
            Some(j) => j,
            None => return None,
        };
        let faster = mine.worse_than(&bl) && bl.mean().saturating_mul(TIE) < mine.mean().saturating_mul(10);
        if !self.held_at(now, hold) && faster { Some(order[j]) } else { None }
    }
}

} // verus!
