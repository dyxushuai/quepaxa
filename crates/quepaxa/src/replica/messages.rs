//! Messages: encoding and sending (gated on durability where they report state), accepting
//! and dispatching received ones, and catching up from peers after a gap or a restart.

use super::*;

impl Replica {
    pub(super) fn encode(&self, to: NodeId, body: Body) -> Outbound {
        let env = Envelope {
            cluster_id: self.wire.cluster_id,
            config_epoch: 0,
            from: self.me,
            to,
            commit_index: self.committed,
        };
        Outbound { to, bytes: Message { env, body }.encode() }
    }

    /// A send that does not depend on durable state.
    pub(super) fn send(&mut self, to: NodeId, body: Body) {
        if to == self.me {
            self.inbox.push_back((to, body));
            return;
        }
        let out = self.encode(to, body);
        self.gate.send(out, &mut self.actions);
    }

    /// A send that reports state possibly not yet durable.
    pub(super) fn send_gated(&mut self, to: NodeId, body: Body) {
        let out = self.encode(to, body);
        self.gate.send_after_durable(out, &mut self.actions);
    }

    pub(super) fn on_message(&mut self, bytes: &Bytes) {
        let Ok(msg) = Message::decode(bytes, &self.wire) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        let from = msg.env.from;
        let known = self.cfg.members.contains(&from) || self.schedule.knows(from);
        // Anyone may read: a joining or removed replica asks members it may not know yet.
        let request = matches!(
            msg.body,
            Body::RecordRequest { .. } | Body::CatchUp { .. } | Body::Forward { .. } | Body::ReadQuery { .. }
        );
        if msg.env.to != self.me || from == self.me || !(known || request) {
            self.metrics.messages_rejected += 1;
            return;
        }
        // Progress hints only from replicas of a known epoch.
        if !known {
            self.inbox.push_back((from, msg.body));
            return;
        }
        if msg.env.commit_index > self.peer_committed {
            self.peer_committed = msg.env.commit_index;
            self.ahead_peer = Some(from);
        }
        if self.peer_committed > self.committed {
            self.seen.entry(Slot(self.committed)).or_insert(self.now);
        }
        if msg.env.commit_index > self.committed + 1 && self.catch_up_due() {
            self.request_catch_up(from);
        }
        if let Body::Entries { .. } = msg.body {
            if self.startup_probe.is_some() {
                self.probe_answers.insert(from);
            }
            self.note_progress(from, msg.env.commit_index);
        }
        self.inbox.push_back((from, msg.body));
    }

    /// Ends the startup probe once a majority of the members at the decided prefix, counting
    /// this replica, answered it. Checked after a page is applied: a page that crosses into a
    /// new epoch must be answered by its members, not the old ones.
    pub(super) fn check_probe_answers(&mut self) {
        if self.startup_probe.is_none() {
            return;
        }
        let members = self.schedule.members(Slot(self.committed));
        let answered = members.iter().filter(|m| **m == self.me || self.probe_answers.contains(m)).count();
        if answered > members.len() / 2 {
            self.startup_probe = None;
            self.probe_answers.clear();
        }
    }

    /// Whether a bulk catch-up request may be (re)sent now.
    pub(super) fn catch_up_due(&self) -> bool {
        self.catch_up.is_none_or(|(_, t)| t.after(self.cfg.retransmit_interval) <= self.now)
    }

    /// Asks every known replica where the log is (again on the retransmit timer, until a
    /// majority of the members answered; see `check_probe_answers`).
    pub(super) fn probe_peers(&mut self) {
        let peers: alloc::collections::BTreeSet<NodeId> = self
            .cfg
            .members
            .iter()
            .copied()
            .chain(self.schedule.everyone())
            .filter(|&m| m != self.me)
            .collect();
        // Nobody to ask (a single member): nothing to wait for either.
        self.startup_probe = (!peers.is_empty()).then(|| self.now.after(self.cfg.retransmit_interval));
        for peer in peers {
            self.send(peer, Body::CatchUp { from: Slot(self.committed) });
        }
    }

    /// Fetches the decided prefix from `committed` in bulk.
    pub(super) fn request_catch_up(&mut self, peer: NodeId) {
        self.catch_up = Some((peer, self.now));
        self.send(peer, Body::CatchUp { from: Slot(self.committed) });
    }

    pub(super) fn pump(&mut self) {
        loop {
            while let Some((from, body)) = self.inbox.pop_front() {
                match body {
                    Body::RecordRequest { slot, step, proposal } => {
                        self.on_request(from, slot, step, proposal)
                    }
                    Body::RecordReply { slot, req_step, step, first, prev_agg } => {
                        self.on_reply(from, slot, req_step, step, first, prev_agg)
                    }
                    Body::Decided { slot, proposal } => self.learn(slot, proposal, false),
                    Body::CatchUp { from: start } => self.on_catch_up(from, start),
                    Body::Entries { first, proposals } => {
                        let page = !proposals.is_empty();
                        for (i, p) in proposals.into_iter().enumerate() {
                            let Some(slot) = first.0.checked_add(i as u64) else { break };
                            self.learn(Slot(slot), p, false);
                        }
                        self.check_probe_answers();
                        // A late or duplicate page from another peer is still useful, but only
                        // the active request's answer moves the catch-up along.
                        if self.catch_up.is_some_and(|(peer, _)| peer == from) {
                            if page {
                                self.catch_up = None;
                                // Still behind after a page: ask the same peer for the next one.
                                if self.peer_committed > self.committed + 1 {
                                    self.request_catch_up(from);
                                }
                            } else {
                                // Nothing new there: ask again only on the timer.
                                self.catch_up = Some((from, self.now));
                            }
                        }
                    }
                    Body::Forward { batch } => self.on_forward(&batch),
                    Body::Compacted { through } => self.on_compacted(from, through),
                    Body::ReadQuery { incarnation, round } => self.on_read_query(from, incarnation, round),
                    Body::ReadReply { incarnation, round, high } => {
                        self.on_read_reply(from, incarnation, round, high)
                    }
                }
            }
            // Before driving: a retirement barrier it submits is proposed in this round.
            self.check_retired();
            self.check_reads();
            self.drive();
            if self.inbox.is_empty() {
                break;
            }
        }
    }
}
