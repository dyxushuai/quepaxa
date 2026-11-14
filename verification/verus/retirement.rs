// crates/quepaxa/src/replica/retire.rs (`Retirement`, `removal`, `Replica::check_retired`)
// ported to Verus (verification/verus/prove.sh checks it).
//
// The port keeps the original's logic statement by statement, except where marked DEV.
// Node ids, epochs, proposal ids and timestamps are u64. Each call that reads the schedule
// gets the latest epoch, its first slot and its members; epochs never go back, and an epoch
// always has the same members. Ghost history records every barrier submitted (id → epoch),
// every own proposal committed, every retirement reported, and fencing.
//
// Proved, for every sequence of calls (fresh barrier ids, none taken once fenced):
// (a) the phase is monotonic: for the latest epoch, Idle < Draining < Confirmed < Retired
//     never goes back except by fencing (which abandons the barrier in flight, and after
//     which none is taken); retirements are reported once each, for ever larger epochs, and
//     once retired from the latest epoch nothing is submitted or reported until a later one;
// (b) a replica retires from an epoch only when that epoch is the latest, excludes it, and
//     a barrier it submitted while removed from that epoch has committed.
//
// Departures from the original's code, forced by what vstd specifies, are marked DEV;
// VERIFICATION.md lists them. When retire.rs changes, port the change here.
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
fn contains(members: &Vec<u64>, me: u64) -> (r: bool)
    ensures r == members@.contains(me),
{
    let mut i = 0;
    while i < members.len()
        invariant i <= members.len(), forall|j: int| 0 <= j < i ==> members@[j] != me,
        decreases members.len() - i,
    {
        if members[i] == me {
            assert(members@[i as int] == me);
            return true;
        }
        i += 1;
    }
    false
}

pub enum Phase {
    Idle,
    Draining { epoch: u64, barrier: u64 },
    Confirmed { epoch: u64 },
    Retired { epoch: u64 },
}

pub enum Next {
    Nothing,
    Retire { epoch: u64 },
    Submit { epoch: u64 },
    Ask(Vec<u64>),
}

/// How far retirement from epoch `l` has come.
pub open spec fn stage(p: Phase, l: u64) -> int {
    match p {
        Phase::Draining { epoch, .. } => if epoch == l { 1 } else { 0 },
        Phase::Confirmed { epoch } => if epoch == l { 2 } else { 0 },
        Phase::Retired { epoch } => if epoch == l { 3 } else { 0 },
        Phase::Idle => 0,
    }
}

/// `removal`: the latest epoch and its first slot, if `me` is not one of its members.
// DEV: `Removal` carries its members; here they are passed beside it.
pub fn removal(epoch: u64, from: u64, members: &Vec<u64>, me: u64) -> (r: Option<(u64, u64)>)
    ensures r == if epoch > 0 && !members@.contains(me) { Some((epoch, from)) } else { None::<(u64, u64)> },
{
    if epoch > 0 && !contains(members, me) { Some((epoch, from)) } else { None }
}

// DEV: `entry(member).or_insert(0)` and `max` as a scan.
fn raise(progress: &mut Vec<(u64, u64)>, member: u64, commit_index: u64) {
    let mut i = 0;
    while i < progress.len()
        invariant i <= progress.len(),
        decreases progress.len() - i,
    {
        if progress[i].0 == member {
            let p = progress[i].1;
            progress.set(i, (member, if p >= commit_index { p } else { commit_index }));
            return;
        }
        i += 1;
    }
    progress.push((member, commit_index));
}

/// A barrier taken while removed from `epoch` has committed.
pub open spec fn barrier_committed(barriers: Map<u64, u64>, commits: Set<u64>, epoch: u64) -> bool {
    exists|b: u64| #[trigger] barriers.contains_key(b) && barriers[b] == epoch && commits.contains(b)
}

pub struct Retirement {
    pub phase: Phase,
    // DEV: a vector of (member, progress) for the `BTreeMap`.
    pub progress: Vec<(u64, u64)>,
    pub probe: Option<u64>,
    pub me: Ghost<u64>,
    /// The latest epoch and its members, as of the last call.
    pub latest: Ghost<u64>,
    pub latest_members: Ghost<Seq<u64>>,
    /// Barriers taken: id to the epoch removed from.
    pub barriers: Ghost<Map<u64, u64>>,
    /// Own proposals committed.
    pub commits: Ghost<Set<u64>>,
    /// Epochs retirement was reported for, in order.
    pub reported: Ghost<Seq<u64>>,
    pub fenced: Ghost<bool>,
}

impl Retirement {
    /// The latest epoch excludes this replica.
    pub open spec fn excluded(self) -> bool {
        self.latest@ > 0 && !self.latest_members@.contains(self.me@)
    }

    /// A barrier taken while removed from `epoch` has committed.
    pub open spec fn confirmed(self, epoch: u64) -> bool {
        barrier_committed(self.barriers@, self.commits@, epoch)
    }

    /// Above every epoch reported.
    pub open spec fn unreported(self, epoch: u64) -> bool {
        self.reported@.len() == 0 || self.reported@.last() < epoch
    }

    pub open spec fn inv(self) -> bool {
        &&& forall|i: int, j: int| 0 <= i < j < self.reported@.len() ==>
                #[trigger] self.reported@[i] < #[trigger] self.reported@[j]
        &&& forall|b: u64| #[trigger] self.barriers@.contains_key(b) ==> self.barriers@[b] <= self.latest@
        &&& match self.phase {
            Phase::Idle => self.reported@.len() == 0 || self.reported@.last() < self.latest@,
            Phase::Draining { epoch, barrier } => {
                &&& self.barriers@.contains_key(barrier) && self.barriers@[barrier] == epoch
                &&& !self.commits@.contains(barrier)
                &&& epoch <= self.latest@ && self.unreported(epoch)
                &&& epoch == self.latest@ ==> self.excluded()
                &&& !self.fenced@
            },
            Phase::Confirmed { epoch } => {
                &&& self.confirmed(epoch)
                &&& epoch <= self.latest@ && self.unreported(epoch)
                &&& epoch == self.latest@ ==> self.excluded()
            },
            Phase::Retired { epoch } => {
                &&& self.confirmed(epoch)
                &&& self.reported@.len() > 0 && self.reported@.last() == epoch
                &&& epoch <= self.latest@
            },
        }
    }

    /// Every call that reads the schedule: epochs never go back, nor change their members.
    pub open spec fn fits(self, epoch: u64, members: Seq<u64>) -> bool {
        &&& epoch >= self.latest@
        &&& epoch == self.latest@ ==> members == self.latest_members@
        &&& epoch == 0 ==> self.latest@ == 0
    }

    pub fn new(me: Ghost<u64>, members: Ghost<Seq<u64>>) -> (s: Retirement)
        ensures s.inv(), s.me == me, s.latest@ == 0, s.latest_members == members, s.phase is Idle,
    {
        Retirement {
            phase: Phase::Idle,
            progress: Vec::new(),
            probe: None,
            me,
            latest: Ghost(0),
            latest_members: members,
            barriers: Ghost(Map::empty()),
            commits: Ghost(Set::empty()),
            reported: Ghost(Seq::empty()),
            fenced: Ghost(false),
        }
    }

    /// Records the latest epoch the caller read from the schedule.
    fn observe(&mut self, epoch: u64, members: Ghost<Seq<u64>>)
        requires old(self).inv(), old(self).fits(epoch, members@),
        ensures
            final(self).inv(), final(self).latest@ == epoch, final(self).latest_members@ == members@,
            final(self).phase == old(self).phase, final(self).reported == old(self).reported,
            final(self).barriers == old(self).barriers, final(self).commits == old(self).commits,
            final(self).fenced == old(self).fenced, final(self).me == old(self).me,
            final(self).progress == old(self).progress, final(self).probe == old(self).probe,
            stage(final(self).phase, epoch) >= stage(old(self).phase, epoch),
    {
        self.latest = Ghost(epoch);
        self.latest_members = members;
    }

    /// The removal still to retire from: none once retired from that epoch.
    // DEV: `filter` and `!=` on `Phase` as a match.
    fn pending(&self, removal: Option<(u64, u64)>) -> (r: Option<(u64, u64)>)
        ensures
            r == match (removal, self.phase) {
                (Some((e, _)), Phase::Retired { epoch }) => if e == epoch { None } else { removal },
                _ => removal,
            },
    {
        match removal {
            Some((e, _)) => match self.phase {
                Phase::Retired { epoch } => if e == epoch { None } else { removal },
                _ => removal,
            },
            None => None,
        }
    }

    // DEV: a scan for `BTreeMap::get`.
    fn progress_of(&self, m: u64) -> u64 {
        let mut i = 0;
        while i < self.progress.len()
            invariant i <= self.progress.len(),
            decreases self.progress.len() - i,
        {
            if self.progress[i].0 == m {
                return self.progress[i].1;
            }
            i += 1;
        }
        0
    }

    /// `note_progress`, with the latest epoch read from the schedule.
    pub fn note_progress(&mut self, me: u64, epoch: u64, from: u64, members: &Vec<u64>, member: u64, commit_index: u64)
        requires old(self).inv(), old(self).fits(epoch, members@), me == old(self).me@,
        ensures
            final(self).inv(), final(self).latest@ == epoch,
            final(self).phase == old(self).phase, final(self).reported == old(self).reported,
            final(self).barriers == old(self).barriers, final(self).commits == old(self).commits,
            final(self).fenced == old(self).fenced, final(self).me == old(self).me,
    {
        self.observe(epoch, Ghost(members@));
        let removal = removal(epoch, from, members, me);
        if self.pending(removal).is_some() {
            raise(&mut self.progress, member, commit_index);
        }
    }

    /// `check`, with the latest epoch read from the schedule.
    pub fn check(&mut self, me: u64, epoch: u64, from: u64, members: &Vec<u64>, now: u64, interval: u64) -> (r: Next)
        requires old(self).inv(), old(self).fits(epoch, members@), me == old(self).me@,
        ensures
            final(self).inv(), final(self).latest@ == epoch,
            final(self).latest_members@ == members@,
            old(self).phase == (Phase::Retired { epoch }) ==> r is Nothing && final(self).phase == old(self).phase,
            final(self).barriers == old(self).barriers, final(self).commits == old(self).commits,
            final(self).fenced == old(self).fenced, final(self).me == old(self).me,
            stage(final(self).phase, epoch) >= stage(old(self).phase, epoch),
            // (b) Retired only from the latest epoch, which excludes it, its barrier committed.
            r matches Next::Retire { epoch: e } ==> {
                &&& e == epoch && epoch > 0 && !members@.contains(final(self).me@)
                &&& old(self).unreported(e)
                &&& final(self).confirmed(e)
                &&& final(self).reported@ == old(self).reported@.push(e)
                &&& final(self).phase == (Phase::Retired { epoch: e })
            },
            !(r is Retire) ==> final(self).reported == old(self).reported,
            // A barrier is asked for only while removed from the latest epoch, not retired
            // from it, and with none in flight.
            r matches Next::Submit { epoch: e } ==> {
                &&& e == epoch && epoch > 0 && !members@.contains(final(self).me@)
                &&& (final(self).phase is Idle || (final(self).phase matches Phase::Retired { epoch: x } && x < e))
            },
    {
        self.observe(epoch, Ghost(members@));
        let removal = removal(epoch, from, members, me);
        if let Phase::Confirmed { epoch: confirmed } = self.phase {
            let ghost reported = self.reported@;
            self.phase = Phase::Idle;
            // DEV: `is_some_and` as a match.
            let retire = match self.pending(removal) {
                Some((e, _)) => e == confirmed,
                None => false,
            };
            if retire {
                self.phase = Phase::Retired { epoch: confirmed };
                self.reported = Ghost(self.reported@.push(confirmed));
                self.progress.clear();
                self.probe = None;
                proof {
                    assert forall|i: int, j: int| 0 <= i < j < self.reported@.len() implies
                        #[trigger] self.reported@[i] < #[trigger] self.reported@[j] by {
                        if j == reported.len() { assert(self.reported@[i] == reported[i]); }
                    }
                }
                return Next::Retire { epoch: confirmed };
            }
        }
        let (epoch, from) = match self.pending(removal) {
            Some(r) => r,
            None => {
                self.progress.clear();
                self.probe = None;
                return Next::Nothing;
            },
        };
        let mut caught_up: usize = 0;
        let mut i = 0;
        while i < members.len()
            invariant i <= members.len(), caught_up <= i,
            decreases members.len() - i,
        {
            if self.progress_of(members[i]) >= from {
                caught_up += 1;
            }
            i += 1;
        }
        if caught_up > members.len() / 2 {
            self.probe = None;
            return match self.phase {
                Phase::Draining { .. } => Next::Nothing,
                _ => Next::Submit { epoch },
            };
        }
        // DEV: `is_none_or` as a match.
        let due = match self.probe {
            None => true,
            Some(t) => t <= now,
        };
        if due {
            self.probe = Some(after_exec(now, interval));
            let mut ask = Vec::new();
            let mut i = 0;
            while i < members.len()
                invariant i <= members.len(),
                decreases members.len() - i,
            {
                if self.progress_of(members[i]) < from {
                    ask.push(members[i]);
                }
                i += 1;
            }
            return Next::Ask(ask);
        }
        Next::Nothing
    }

    /// `submitted`: the barrier `check` asked for was taken (a fresh id).
    pub fn submitted(&mut self, epoch: u64, barrier: u64)
        requires
            old(self).inv(), !old(self).fenced@,
            epoch == old(self).latest@ && old(self).excluded(),
            old(self).phase is Idle || (old(self).phase matches Phase::Retired { epoch: x } && x < epoch),
            !old(self).barriers@.contains_key(barrier), !old(self).commits@.contains(barrier),
        ensures
            final(self).inv(), final(self).latest == old(self).latest,
            final(self).reported == old(self).reported, final(self).commits == old(self).commits,
            final(self).fenced == old(self).fenced, final(self).me == old(self).me,
            final(self).latest_members == old(self).latest_members,
            final(self).barriers@ == old(self).barriers@.insert(barrier, epoch),
            stage(final(self).phase, epoch) >= stage(old(self).phase, epoch),
    {
        self.barriers = Ghost(self.barriers@.insert(barrier, epoch));
        self.phase = Phase::Draining { epoch, barrier };
    }

    /// `committed`: own proposal `id` committed; true if it was the barrier.
    pub fn committed(&mut self, id: u64) -> (r: bool)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).latest == old(self).latest,
            final(self).latest_members == old(self).latest_members,
            final(self).reported == old(self).reported, final(self).barriers == old(self).barriers,
            final(self).fenced == old(self).fenced, final(self).me == old(self).me,
            final(self).commits@ == old(self).commits@.insert(id),
            stage(final(self).phase, old(self).latest@) >= stage(old(self).phase, old(self).latest@),
    {
        let ghost b = match self.phase { Phase::Draining { barrier, .. } => barrier, _ => 0 };
        self.commits = Ghost(self.commits@.insert(id));
        match self.phase {
            Phase::Draining { epoch, barrier } => {
                if barrier == id {
                    self.phase = Phase::Confirmed { epoch };
                    proof { assert(self.barriers@.contains_key(b) && self.commits@.contains(b)); }
                    return true;
                }
                false
            },
            _ => false,
        }
    }

    /// `fence`: the barrier in flight, if any, is abandoned and returned.
    pub fn fence(&mut self) -> (r: Option<u64>)
        requires old(self).inv(),
        ensures
            final(self).inv(), final(self).fenced@, final(self).latest == old(self).latest,
            final(self).latest_members == old(self).latest_members,
            final(self).reported == old(self).reported, final(self).barriers == old(self).barriers,
            final(self).commits == old(self).commits, final(self).me == old(self).me,
            final(self).phase == old(self).phase || final(self).phase is Idle,
    {
        self.fenced = Ghost(true);
        match self.phase {
            Phase::Draining { barrier, .. } => {
                self.phase = Phase::Idle;
                Some(barrier)
            },
            _ => None,
        }
    }
}

/// `Replica::check_retired`. DEV: `me` (the replica's) is passed in; `submit`'s outcome is an input, `taken` (a fresh id, or
/// none when refused: busy or fenced); the catch-up requests are returned, not sent.
pub fn check_retired(
    r: &mut Retirement,
    me: u64,
    epoch: u64,
    from: u64,
    members: &Vec<u64>,
    now: u64,
    interval: u64,
    taken: Option<u64>,
) -> (next: Next)
    requires
        old(r).inv(), old(r).fits(epoch, members@), me == old(r).me@,
        taken matches Some(id) ==> !old(r).barriers@.contains_key(id) && !old(r).commits@.contains(id),
        old(r).fenced@ ==> taken is None,
    ensures
        final(r).inv(), final(r).latest@ == epoch, final(r).me == old(r).me,
        stage(final(r).phase, epoch) >= stage(old(r).phase, epoch),
        // (a) Reported once each, for ever larger epochs.
        next matches Next::Retire { epoch: e } ==> final(r).reported@ == old(r).reported@.push(e)
            && (old(r).reported@.len() == 0 || old(r).reported@.last() < e),
        !(next is Retire) ==> final(r).reported == old(r).reported,
        // Once retired from the latest epoch, nothing more until a later one.
        old(r).phase == (Phase::Retired { epoch }) ==> next is Nothing && final(r).phase == old(r).phase,
        // (b)
        next matches Next::Retire { epoch: e } ==>
            e == epoch && epoch > 0 && !members@.contains(final(r).me@) && final(r).confirmed(e),
{
    let next = r.check(me, epoch, from, members, now, interval);
    if let Next::Submit { epoch } = next {
        if let Some(id) = taken {
            r.submitted(epoch, id);
        }
    }
    next
}

} // verus!
