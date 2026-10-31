//! Carrying out a replica's actions: the network, the disks, the application and the
//! oracles that check every commit.

use super::*;

impl Sim {
    pub(crate) fn net_send(&mut self, from: usize, out: Outbound) {
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
            n.replica = None;
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

    pub(crate) fn on_action(&mut self, i: usize, a: Action) {
        match a {
            Action::Send(out) => self.net_send(i, out),
            Action::Persist { id, records, then, replace } => {
                // Group commit: writes issued while an fsync runs share the next one.
                let lat = 1 + self.rng.below(self.cfg.max_persist_ms * MS);
                let n = &mut self.nodes[i];
                if n.disk_next_group.is_some_and(|g| g <= self.now) || self.now >= n.disk_busy_until {
                    n.disk_busy_until = n.disk_busy_until.max(n.disk_next_group.unwrap_or(0));
                    n.disk_next_group = None;
                }
                let done = if self.now >= n.disk_busy_until {
                    n.disk_busy_until = self.now + lat;
                    n.disk_busy_until
                } else {
                    *n.disk_next_group.get_or_insert(n.disk_busy_until + lat)
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
                    self.report.commit_latencies_us.push((self.now - at) / 1_000);
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
                self.kv.read_dropped(i, id);
            }
            Action::Reconfigured { epoch, members, from } => {
                let _ = writeln!(self.trace, "{} n{i} epoch {epoch} from {}: {members:?}", self.now, from.0);
                let next: Vec<usize> = members.iter().map(|m| m.0 as usize).collect();
                if let Some(seen) = self.epochs.get(&epoch) {
                    assert!(
                        *seen == (from.0, next.clone()),
                        "{}",
                        self.fail(&format!("epoch {epoch} disagrees: {seen:?} vs from {} {next:?}", from.0))
                    );
                    return;
                }
                self.report.reconfigurations += 1;
                self.epochs.insert(epoch, (from.0, next.clone()));
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
                let back =
                    self.epochs.range(epoch + 1..).next_back().is_some_and(|(_, (_, m))| m.contains(&i));
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
        while let Some((from, next)) = self.epochs.get(&(self.epoch + 1)).cloned() {
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
}
