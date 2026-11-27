#!/usr/bin/env bash
# Checks the Verus proofs on ports of the replica's code, then plants known bugs that the
# proofs must reject: the durability gate (gate.rs, from crates/quepaxa/src/replica/gate.rs)
# never lets a send or effect overtake the writes it depends on; commit-time deduplication
# (dedup.rs, from crates/quepaxa/src/dedup.rs) applies an entry at most once; the leader
# schedule (schedule.rs, from crates/quepaxa/src/schedule.rs) never changes the order of a
# slot within LAG of the slots fed, and a change computed from an older generation installs
# nothing; the disk's health (disk_health.rs, from crates/quepaxa/src/replica/disk.rs and
# watch.rs) is stuck only when no write completed for δ while one was in flight, holds
# hedges only during a shared stall, counts a busy target's lateness from its last
# decision only for an acknowledged forward and never past 4 hedge_max after it, and steps
# aside and back in turn; leader placement
# (placement.rs, from crates/quepaxa/src/placement_policy.rs) never places an excluded
# member first, explores each tied member once until its measurement goes stale, measures
# leader and candidates by one rule, and resolves its move before anything else; retirement
# (retirement.rs, from crates/quepaxa/src/replica/retire.rs) never goes back for the latest
# epoch, and retires only once its barrier committed in an epoch that excludes it; where
# entries wait (entries.rs, from crates/quepaxa/src/replica/entries.rs and entries/back.rs)
# keeps every own entry in the place its state names, loses none, counts what the replica
# asks in O(1) correctly, routes everything pending when not proposing directly, and hands
# back only entries not yet committed, and none of another member's from a batch handed on
# when stepping aside; recovery (recovery.rs, from crates/quepaxa/src/replica/recovery.rs)
# never lets a restarted replica propose at the top priority in a slot it may have reserved
# before the crash, rejects a recovered reservation through u64::MAX rather than saturate
# it, leaves Recovering only once a majority answered, and stops catching up
# only once caught up with its peers and its read mark. Uses
# Verus, fetched (pinned, checksum verified) into target/verus-<digest>, and the Rust
# toolchain that
# release requires (installed with rustup if missing). Linux x86_64 only.
#
# Usage: verification/verus/prove.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
version=0.2026.10.04.426d8b0
sha256=624018662868563a7bec0dd07be5e4dd0c3d61124b4b55105765884bf2d4d1a0
toolchain=1.98.1-x86_64-unknown-linux-gnu
url=https://github.com/verus-lang/verus/releases/download/release/$version/verus-$version-x86-linux.zip
# The digest in the path: a new pin never reuses an install of another archive.
tools=$root/target/verus-${sha256:0:12}
work=$root/target/verus-work

if [[ ! -x $tools/verus ]]; then
  mkdir -p "$root/target"
  curl -fsSL -o "$tools.zip.part" "$url"
  [[ $(sha256sum "$tools.zip.part" | cut -d' ' -f1) == "$sha256" ]] || {
    echo "Verus archive checksum mismatch"; rm -f "$tools.zip.part"; exit 1; }
  rm -rf "$tools.unpack" && mkdir "$tools.unpack"
  unzip -q "$tools.zip.part" -d "$tools.unpack"
  rm -rf "$tools" && mv "$tools.unpack/verus-x86-linux" "$tools"
  rm -rf "$tools.unpack" "$tools.zip.part"
fi
rustup run "$toolchain" rustc --version >/dev/null 2>&1 ||
  rustup toolchain install "$toolchain" --profile minimal

verus() { "$tools/verus" --crate-type=lib "$@"; }

verus "$here/gate.rs"
verus "$here/dedup.rs"
verus "$here/schedule.rs"
verus "$here/disk_health.rs"
verus "$here/placement.rs"
verus "$here/retirement.rs"
verus "$here/entries.rs"
verus "$here/recovery.rs"

# Each bug: the file, a name, then a sed expression that plants it. The proof must fail on
# every one.
bugs=(
  gate  reply-before-own-writes-durable  '0,/if self\.idle\(\)/s//if self.ready()/'
  gate  park-behind-oldest-persist       's/self\.in_flight\[n - 1\]\./self.in_flight[0]./g'
  gate  release-one-persist-too-many     's/self\.in_flight\[0\]\.id > id \{/self.in_flight[0].id > id + 1 {/'
  gate  persist-drops-its-records        's/actions\.push\(Action::Persist \{ id, records,/actions.push(Action::Persist { id, records: Vec::new(),/'
  gate  checkpoint-appends               's/        self\.replace = true;/        self.replace = false;/'
  dedup evicted-incarnation-reapplied    's/            if min_key_gt\(incs, incarnation\) \{/            if false \&\& min_key_gt(incs, incarnation) {/'
  dedup id-below-window-reapplied        's/let fresh = id >= w\.below \&\& /let fresh = /'
  dedup window-skips-an-id               's/w\.below = w\.below \+ 1;/w.below = w.below + 2;/'
  dedup contains-forgets-evictions       's/None => incs\.len\(\) >= INCARNATIONS_KEPT \&\& min_key_gt\(incs, incarnation\),/None => false,/'
  schedule rotation-takes-effect-early   '0,/let from = slot \+ LAG;/s//let from = slot + LAG - 1;/'
  schedule change-takes-effect-early     '/fn reconfigure/,$ s/let from = slot \+ LAG;/let from = slot + LAG - 1;/'
  schedule prune-drops-the-window        's/let keep_from = self\.order_index\(slot\);/let keep_from = self.order_index(slot + LAG);/'
  schedule stale-change-applied         's/if base != self\.generation \|\| /if /'
  schedule rotation-not-counted         '0,/self\.generation = self\.generation \+ 1;/s///'
  schedule change-not-counted           '/fn reconfigure/,$ s/self\.generation = self\.generation \+ 1;//'
  schedule change-in-a-rotated-slot     's/ \|\| rotated_here//'
  schedule hedge-rotates-the-leader     's/if hedge \{/if false {/'
  disk_health stuck-counted-from-the-oldest-write 's/max_exec\(oldest, self\.wrote\)/oldest/'
  disk_health shared-stall-despite-a-decision     's/self\.decided_elsewhere <= after_exec\(oldest, usual4\)/self.decided_elsewhere <= now/'
  disk_health hold-on-a-forecast                  's/match disk\.health\(now, d\)/match disk.health(late, d)/'
  disk_health resumption-not-recorded             's/^            self\.resumed = now;$//'
  disk_health step-aside-twice                    's/if aside == self\.aside \{/if false \&\& aside == self.aside {/'
  disk_health busy-counted-from-the-forward       's/let from = if busy_now \&\& acked \{ progressed \} else \{ at \};/let from = at;/'
  disk_health progress-before-the-ack             's/if busy_now \&\& acked \{/if busy_now {/'
  disk_health no-hard-bound                       's/let late = if unbounded <= bound \{ unbounded \} else \{ bound \};/let late = unbounded;/'
  placement excluded-member-placed-first  's/&& !self\.excluded_at\(order\[i\], now\),/\&\& true,/'
  placement demoted-member-explored       's/&& !self\.demoted_at\(order\[i\], now, hold\),/\&\& true,/'
  placement leader-measured-by-span-only  's/if !mine\.measured\(now, sustain, stale\) \{/if !mine.spans(sustain) {/'
  placement measured-member-explored      's/^                && !self\.measured_at\(order\[i\], now, sustain, stale\) \{/                {/'
  placement exploit-within-a-hold         's/if !self\.held_at\(now, hold\) && faster/if faster/'
  placement move-ignores-the-hold         's/ \|\| self\.held_at\(now, hold\) \{/ {/'
  placement samples-from-a-past-tenure    '/fn start_tenure/,/^    \}/ s/self\.led\.remove\(&leader\);//'
  placement move-never-resolves           '/fn on_resolved/,/^    \}/ s/self\.pending = Pending::Idle;//'
  placement untrack-drops-the-move        's/if let Pending::Tracking\(_\) = self\.pending \{/if true {/'
  placement restart-keeps-the-tenure      '/fn on_restart/,/^    \}/ s/self\.start_tenure\(leader, now\);/self.tenure.leader = leader; self.led.remove(\&leader);/'
  retirement retirement-forgotten               's/^ +self\.phase = Phase::Retired \{ epoch: confirmed \};//'
  retirement second-barrier-in-flight           's/^ +Phase::Draining \{ \.\. \} => Next::Nothing,//'
  retirement retired-although-added-back        's/None => false,/None => true,/'
  retirement old-barrier-retires-a-later-epoch  's/=> e == confirmed,/=> e >= confirmed,/'
  retirement confirmed-by-another-commit        's/if barrier == id \{/if true {/'
  retirement observer-retires                   's/if !was_member \{/if false {/'
  entries start-forgets-unslotted        's/^        self\.unslotted_remove\(seq\);$//'
  entries route-skips-forwarded-insert   's/^                self\.fwd_insert\(e, now\);$//'
  entries suspect-drops-the-last-entry   's/^            self\.pending\.insert\(0, e\);$/            if f.len() > 0 { self.pending.insert(0, e); }/'
  entries lost-lead-takes-slotted-batches '0,/if self\.batches\[j\]\.slot\.is_none\(\) \{/s//if true {/'
  entries step-aside-leaves-relayed      's/while self\.relayed\.len\(\) > 0/while false/'
  entries lost-slot-skips-dedup          's/if dedup\.contains\(x\.origin, x\.inc, x\.id\) \{/if false {/'
  entries received-forgets-foreign       's/^            self\.foreign = self\.foreign \+ f;$//'
  entries lost-slot-ignores-handed       's/^        let handed = b\.handed;$/        let handed = false;/'
  entries step-aside-forgets-handed      's/^                self\.set_handed\(j\);$//'
  recovery restart-leads-below-its-reservation 's/let leader = leads \&\& self\.res\.may_lead\(slot\);/let leader = leads;/'
  recovery restart-ignores-the-reservation     's/match Reservation::recovered\(recovered\) \{/match Reservation::recovered(None) {/'
  recovery recovered-reservation-saturates     's/if r < u64::MAX \{ r \+ 1 \} else \{ return None; \}/if r < u64::MAX { r + 1 } else { r }/'
  recovery top-request-not-held                's/let gate = step == 4 \&\& top \&\& !h_ready \&\& !to_me;/let gate = false;/'
  recovery reservation-durable-at-once         's/^                self\.waits\.push\(Wait::Durable\(through\)\);$/                self.res.made_durable(through);/'
  recovery checkpoint-drops-the-reservation    's/None => if self\.floor > 0 \{ Some\(self\.floor - 1\) \} else \{ None \},/None => None,/'
  recovery recovered-on-any-answer             's/if n > members\.len\(\) \/ 2 \{/if n > 0 {/'
  recovery fence-undone-by-a-probe             '0,/if !\(self\.probe_at\(\)\.is_some\(\)\) \{/s//if false {/'
  recovery probe-forgets-the-answers           's/Some\(at\) => \*self = Phase::Recovering \{ probe_at: at, answered \},/Some(at) => *self = Phase::Recovering { probe_at: at, answered: Vec::new() },/'
  recovery read-catch-up-stops-below-the-mark  's/if committed < mark \{ Some\(peer\) \} else \{ None \}/if committed < mark \&\& mark - committed > 1 { Some(peer) } else { None }/'
  recovery read-catch-up-asks-a-lower-mark     's/Some\(b\) => answers\[i\]\.1 < answers\[b\]\.1,/Some(b) => true,/'
  recovery timer-ignores-the-read-mark         's/c\.retry\(committed, outside, now, interval, read_from\)/c.retry(committed, outside, now, interval, None)/'
  recovery page-from-another-peer-moves-it     's/Some\(\(p, _\)\) => p == from,/Some((p, _)) => true,/'
  recovery outside-skips-single-slot-lag       's/if outside \{ self\.ahead > committed \} else /if false { self.ahead > committed } else /'
)
mkdir -p "$work"
status=0
for ((i = 0; i < ${#bugs[@]}; i += 3)); do
  file=${bugs[i]} name=${bugs[i + 1]}
  sed -E "${bugs[i + 2]}" "$here/$file.rs" > "$work/$name.rs"
  if cmp -s "$here/$file.rs" "$work/$name.rs"; then
    echo "STALE   $name (the bug no longer applies: refresh it against $file.rs)"; status=1
  elif verus "$work/$name.rs" > "$work/$name.log" 2>&1; then
    echo "MISSED  $name"; status=1
  elif grep -q "verification results:: .* [1-9][0-9]* errors" "$work/$name.log"; then
    echo "CAUGHT  $name"
  else
    echo "BROKEN  $name (did not reach verification: see $work/$name.log)"; status=1
  fi
done
exit $status
