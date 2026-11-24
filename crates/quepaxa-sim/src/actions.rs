//! Carrying out a replica's actions: the network, the disks, the application and the
//! oracles that check every commit.

use super::*;

impl Sim {
    pub(crate) fn net_send(&mut self, from: usize, out: Outbound) {
        self.report.bytes_sent[from] += out.bytes.len() as u64;
        let to = out.to.0 as usize;
        if self.faults_on
            && (self.cut.contains(&from) != self.cut.contains(&to) || self.chance(self.cfg.drop))
        {
            return;
        }
        let copies = if self.faults_on && self.chance(self.cfg.dup) { 2 } else { 1 };
        for _ in 0..copies {
            let base = self.cfg.links_ms.as_ref().map_or(0, |l| l[from][to] * MS);
            let at = self.now + 1 + base + self.rng.below(self.cfg.max_latency_ms * MS);
            self.push(at, Event::Deliver { to, bytes: out.bytes.to_vec() });
        }
    }

    pub(crate) fn drain(&mut self, i: usize) {
        let mut actions = Vec::new();
        let Some(r) = self.nodes[i].replica.as_mut() else {
            return;
        };
        r.poll_actions(&mut actions);
        for a in actions {
            self.on_action(i, a);
        }
        // An operator shuts a removed replica down once it reports `Retired` (the new members
        // no longer need it) and nothing is being written here.
        let n = &mut self.nodes[i];
        if n.retiring && n.pending.is_empty() {
            if let Some(r) = n.replica.take() {
                self.report.entry_probes += r.inspect().entries;
            }
            n.retired = true;
            n.retiring = false;
            n.generation += 1;
            // Clients reading there get no answer, as from any replica that shuts down.
            self.kv.abandon_reads(i);
            let _ = writeln!(self.trace, "{} n{i} retires", self.now);
            return;
        }
        if let Some(r) = self.nodes[i].replica.as_ref() {
            self.report.max_retained_slots = self.report.max_retained_slots.max(r.metrics().retained_slots);
        }
        self.maybe_compact(i);
        self.install_snapshots();
        let Some(r) = self.nodes[i].replica.as_mut() else { return };
        let deadline = r.next_deadline();
        let n = &mut self.nodes[i];
        if let Some(Timestamp(at)) = deadline {
            let at = at.max(self.now);
            if n.timer_at != Some(at) {
                n.timer_at = Some(at);
                let generation = n.generation;
                self.push(at, Event::Timer { node: i, generation, at });
            }
        }
    }

    /// When a disk fault on `i` in force now ends.
    pub(crate) fn during(&self, fault: &Option<(Vec<usize>, u64, u64)>, i: usize) -> Option<u64> {
        match fault {
            Some((nodes, at, ms)) if nodes.contains(&i) && (at * MS..(at + ms) * MS).contains(&self.now) => {
                Some((at + ms) * MS)
            }
            _ => None,
        }
    }

    /// How long `i` takes to handle one message, in virtual ns, if it runs on a slow machine.
    pub(crate) fn cpu_cost(&self, i: usize) -> Option<u64> {
        let (nodes, us) = self.cfg.slow_cpu.as_ref()?;
        nodes.contains(&i).then_some(us * 1_000)
    }

    pub(crate) fn on_action(&mut self, i: usize, a: Action) {
        match a {
            Action::Send(out) => self.net_send(i, out),
            Action::Persist { id, records, then, replace } => {
                // Group commit: writes issued while an fsync runs share the next one.
                let max_ms = if self.during(&self.cfg.disk_slow, i).is_some() {
                    self.cfg.slow_persist_ms
                } else {
                    self.cfg.max_persist_ms
                };
                let lat = 1 + self.rng.below(max_ms * MS);
                let stall = self.during(&self.cfg.disk_stall, i).unwrap_or(0);
                let n = &mut self.nodes[i];
                if n.disk_next_group.is_some_and(|g| g <= self.now) || self.now >= n.disk_busy_until {
                    n.disk_busy_until = n.disk_busy_until.max(n.disk_next_group.unwrap_or(0));
                    n.disk_next_group = None;
                }
                let done = if self.now >= n.disk_busy_until {
                    n.disk_busy_until = self.now.max(stall) + lat;
                    n.disk_busy_until
                } else {
                    match n.disk_next_group {
                        // A group the stall does not hold back takes no new writes; they start
                        // a later one, after both.
                        Some(g) if g < stall => {
                            let later = g.max(stall) + lat;
                            n.disk_busy_until = n.disk_busy_until.max(g);
                            n.disk_next_group = Some(later);
                            later
                        }
                        _ => *n.disk_next_group.get_or_insert(n.disk_busy_until.max(stall) + lat),
                    }
                };
                n.pending.push(PendingPersist { id: id.0, records, then, replace });
                let generation = n.generation;
                self.push(done, Event::PersistDone { node: i, generation, id: id.0 });
            }
            Action::Commit { slot, entries } => self.on_commit(i, slot, entries),
            Action::Committed { id, .. } if self.nodes[i].changes.remove(&id) => {}
            Action::Committed { id, slot } => {
                let n = &mut self.nodes[i];
                if let Some(at) = n.proposed_at.remove(&id) {
                    if self.progress.holds(i) {
                        self.report.held_writes += 1;
                    }
                    self.report.commit_latencies_us.push((self.now - at) / 1_000);
                    self.report.commits.push((i, at / 1_000, self.now / 1_000));
                }
                let payload = n.incarnation_payloads.get(&id).cloned();
                n.outstanding.remove(&id);
                if let Some(p) = &payload {
                    self.kv.write_completed(p);
                }
                let logged = self.ledger.acknowledged(
                    NodeId(i as u32),
                    id,
                    &payload.unwrap_or_default(),
                    slot.0,
                    self.nodes[i].installed,
                );
                if let Err(e) = logged {
                    panic!("{}", self.fail(&e));
                }
            }
            Action::Dropped { id, .. } => {
                self.nodes[i].outstanding.remove(&id);
                self.nodes[i].changes.remove(&id);
                self.kv.read_dropped(i, id);
            }
            Action::Reconfigured { epoch, members, from } => {
                let _ = writeln!(self.trace, "{} n{i} epoch {epoch} from {}: {members:?}", self.now, from.0);
                let next: Vec<usize> = members.iter().map(|m| m.0 as usize).collect();
                if let Some(seen) = self.epochs.applied.get(&epoch) {
                    assert!(
                        *seen == (from.0, next.clone()),
                        "{}",
                        self.fail(&format!("epoch {epoch} disagrees: {seen:?} vs from {} {next:?}", from.0))
                    );
                    return;
                }
                self.report.reconfigurations += 1;
                if self.cfg.reorder.is_some() {
                    self.report.reorder_decided_us.get_or_insert(self.now / 1_000);
                }
                self.epochs.applied.insert(epoch, (from.0, next.clone()));
                // Removed replicas serve until they report `Retired`; returning ones start
                // now so they catch up during the window.
                for &back in &next {
                    let n = &mut self.nodes[back];
                    n.retiring = false;
                    if std::mem::take(&mut n.retired) {
                        let _ = writeln!(self.trace, "{} n{back} rejoins", self.now);
                        self.push(self.now, Event::Restart { node: back });
                    }
                }
            }
            Action::Retired { epoch } => {
                let _ = writeln!(self.trace, "{} n{i} retired from epoch {epoch}", self.now);
                // A replica that has not caught up with a later change adding it back may
                // still report the older epoch; the operator knows better and keeps it.
                let back = self
                    .epochs
                    .applied
                    .range(epoch + 1..)
                    .next_back()
                    .is_some_and(|(_, (_, m))| m.contains(&i));
                self.nodes[i].retiring = !back;
            }
            Action::ReadReady { id, .. } => self.kv.read_ready(i, id),
            Action::SnapshotNeeded { peer, through } => {
                self.pending_installs.push((i, peer.0 as usize, through.0))
            }
            _ => {}
        }
    }

    /// Switches clients, faults and convergence to an announced epoch once the agreed log
    /// reaches its first slot.
    pub(crate) fn activate_epochs(&mut self) {
        while let Some((from, next)) = self.epochs.applied.get(&(self.epoch + 1)).cloned() {
            if (self.ledger.slots() as u64) < from {
                return;
            }
            self.epoch += 1;
            // Clients of a removed replica get no answers; that is not a failure.
            for gone in self.members.clone().into_iter().filter(|m| !next.contains(m)) {
                self.nodes[gone].outstanding.clear();
                self.kv.abandon_reads(gone);
            }
            let _ = writeln!(self.trace, "{} epoch {} in force: {next:?}", self.now, self.epoch);
            if self.cfg.reorder.is_some() {
                self.report.reorder_in_force_us.get_or_insert(self.now / 1_000);
            }
            self.report.epochs.push(next.clone());
            self.members = next;
        }
    }

    pub(crate) fn on_commit(&mut self, i: usize, slot: Slot, entries: Vec<Entry>) {
        let expected = self.nodes[i].next_commit;
        assert!(slot.0 <= expected, "{}", self.fail(&format!("n{i} commit {} skips {expected}", slot.0)));
        let payloads: Vec<Bytes> = entries.iter().map(|e| e.payload.clone()).collect();
        if slot.0 == expected {
            self.nodes[i].next_commit += 1;
            self.kv.apply(i, &payloads);
        }
        let _ = writeln!(self.trace, "{} n{i} commit {} ({} entries)", self.now, slot.0, entries.len());
        match self.ledger.commit(slot.0, &entries) {
            Err(e) => panic!("{}", self.fail(&e)),
            Ok(false) => {}
            Ok(true) => {
                self.kv.first_commit(slot.0, &payloads);
                if self.faults_on && self.cfg.kill.is_some_and(|(_, at)| self.now >= at * MS) {
                    self.report.slots_after_kill += 1;
                }
                if self.faults_on {
                    let gap = (self.now - self.last_new_slot_at) / MS;
                    self.report.max_commit_gap_ms = self.report.max_commit_gap_ms.max(gap);
                }
                self.last_new_slot_at = self.now;
                self.activate_epochs();
            }
        }
    }

    /// The application snapshots its state (durably, instantly in the sim) and compacts.
    pub(crate) fn maybe_compact(&mut self, i: usize) {
        let Some(every) = self.cfg.compact_every else { return };
        let last = self.nodes[i].snapshot.as_ref().map_or(0, |s| s.through + 1);
        let next = self.nodes[i].next_commit;
        if next < last + every {
            return;
        }
        let Some(core) = self.nodes[i].replica.as_ref().map(Replica::core_state) else { return };
        let through = next - 1;
        let state = self.kv.state(i);
        self.nodes[i].snapshot = Some(Snapshot { through, state, core: core.clone() });
        let r = self.nodes[i].replica.as_mut().expect("checked above");
        r.compact(Timestamp(self.now), Slot(through), core).expect("valid compaction");
        self.drain(i);
    }

    /// Ships snapshots requested by `SnapshotNeeded` (out of band, instantly).
    pub(crate) fn install_snapshots(&mut self) {
        while let Some((i, peer, through)) = self.pending_installs.pop() {
            let Some(snap) = self.nodes[peer].snapshot.clone() else { continue };
            if snap.through < through {
                continue;
            }
            let Some(r) = self.nodes[i].replica.as_mut() else { continue };
            // The application adopts the snapshot only if the replica does (never rewinds).
            if !r
                .install_snapshot(Timestamp(self.now), Slot(snap.through), snap.core.clone())
                .expect("valid snapshot")
            {
                continue;
            }
            self.kv.set_state(i, snap.state.clone());
            self.nodes[i].next_commit = snap.through + 1;
            self.nodes[i].snapshot = Some(snap.clone());
            self.nodes[i].installed = Some(snap.through);
            self.report.snapshots_installed += 1;
            let _ = writeln!(self.trace, "{} n{i} install snapshot through {}", self.now, snap.through);
            self.drain(i);
        }
    }

    /// A timer that fires and leaves its deadline in the past with nothing changed fires
    /// again at once, forever: fail on the second such firing rather than spin.
    pub(crate) fn check_timer_storm(&mut self, node: usize) -> Result<(), String> {
        let now = self.now;
        let Some(r) = self.nodes[node].replica.as_ref() else { return Ok(()) };
        let past = r.next_deadline().filter(|d| d.0 <= now).map(|d| (d.0, r.fingerprint()));
        let n = &mut self.nodes[node];
        let Some((deadline, fp)) = past else {
            n.past_deadline = None;
            return Ok(());
        };
        if n.past_deadline.replace((now, fp)) == Some((now, fp)) {
            return Err(self.fail(&format!(
                "timer storm: n{node} keeps firing its deadline {deadline} at {now} without progress"
            )));
        }
        Ok(())
    }

    /// R4: while a majority of members is alive, mutually connected and on working disks,
    /// each write proposed at one of them commits within [`SimConfig::progress_bound_ms`] of
    /// when it was proposed, or of when its replica joined that majority if later. Members
    /// cut off by the partition, down, or with a stalled or slowed disk are outside it.
    pub(crate) fn check_progress(&mut self) -> Result<(), String> {
        let Some(bound) = self.cfg.progress_bound_ms().map(|ms| ms * MS) else { return Ok(()) };
        let healthy: BTreeSet<usize> = self
            .members
            .iter()
            .copied()
            .filter(|&i| {
                self.nodes[i].replica.is_some()
                    && !self.cut.contains(&i)
                    && self.during(&self.cfg.disk_stall, i).is_none()
                    && self.during(&self.cfg.disk_slow, i).is_none()
            })
            .collect();
        self.progress.observe(self.now, &healthy, self.members.len());
        for &i in &healthy {
            let n = &mut self.nodes[i];
            let Some(r) = n.replica.as_ref() else { continue };
            let (emitted, committed) = (r.emitted_index(), r.commit_index());
            if emitted == committed {
                n.emit_stuck = None;
            } else if n.emit_stuck.is_none_or(|(e, _)| e != emitted) {
                n.emit_stuck = Some((emitted, self.now));
            }
        }
        let stuck = self.progress.held().find_map(|(i, since)| {
            let n = &self.nodes[i];
            let oldest = n.outstanding.keys().filter_map(|id| Some((*n.proposed_at.get(id)?, *id))).min();
            oldest.filter(|(at, _)| self.now > (*at).max(since) + bound).map(|(at, id)| (i, at, id))
        });
        if let Some((i, at, id)) = stuck {
            return Err(self.fail(&format!(
                "no progress: n{i}'s write {id:?} proposed at {} ms is not committed {} ms later in connected majority {healthy:?}",
                at / MS,
                bound / MS
            )));
        }
        let emit_stuck = self.progress.held().find_map(|(i, since)| {
            let (emitted, lag_at) = self.nodes[i].emit_stuck?;
            (self.now > lag_at.max(since) + bound).then(|| {
                let committed = self.nodes[i].replica.as_ref().map_or(0, Replica::commit_index);
                (i, emitted, committed)
            })
        });
        if let Some((i, emitted, committed)) = emit_stuck {
            return Err(self.fail(&format!(
                "no progress: n{i}'s emitted_index ({emitted}) stayed behind commit_index ({committed}) for {} ms in connected majority {healthy:?}",
                bound / MS
            )));
        }
        Ok(())
    }

    /// G1, G3–G6 across every replica's state and durable disks, and where each keeps its
    /// entries (`Replica::check_entries`).
    pub(crate) fn check_internal(&mut self) -> Result<(), String> {
        let entries =
            self.nodes.iter().filter_map(|n| n.replica.as_ref()).try_for_each(Replica::check_entries);
        let live: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.replica.as_ref().map(|r| (i, r.inspect())))
            .collect();
        let result = entries
            .and_then(|()| self.history.observe(&self.epochs, &live))
            .and_then(|()| self.custody.check_decided(&self.epochs, self.history.decided()));
        result.map_err(|e| self.fail(&e))
    }
}
