//! Messages: encoding and sending (gated on durability where they report state), and
//! accepting and dispatching received ones.

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
        // A replica outside the latest epoch it knows (joining, removed, or added back) may
        // have lost every replica it knows to later changes: only members of an epoch it has
        // not learned yet can tell it the log, so it listens to anyone in the cluster.
        let known = self.outside() || self.cfg.members.contains(&from) || self.schedule.knows(from);
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
        self.heard_from(from);
        self.watch.committed.insert(from, msg.env.commit_index);
        self.catch_up.heard(from, msg.env.commit_index);
        if self.catch_up.ahead_of(self.committed) {
            self.seen.entry(Slot(self.committed)).or_insert(self.now);
        }
        if msg.env.commit_index > self.committed + 1 && self.catch_up_due() {
            self.request_catch_up(from);
        }
        if let Body::Entries { .. } = msg.body {
            self.phase.answered(from);
            self.note_progress(from, msg.env.commit_index);
        }
        self.inbox.push_back((from, msg.body));
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
                    Body::Decided { slot, priority, key } => self.on_decided(from, slot, priority, key),
                    Body::CatchUp { from: start } => self.on_catch_up(from, start),
                    // Content resolves references, which are refused until rules R and P
                    // (#172): a value holding one never commits its entries.
                    Body::Entries { first, proposals, content: _ } => {
                        let empty = proposals.is_empty();
                        for (i, p) in proposals.into_iter().enumerate() {
                            let Some(slot) = first.0.checked_add(i as u64) else { break };
                            self.learn(Slot(slot), p, false);
                        }
                        self.check_probe_answers();
                        // A late or duplicate page from another peer is still useful, but only
                        // the active request's answer moves the catch-up along.
                        if let Some(peer) = self.catch_up.page(from, empty, self.committed, self.now) {
                            self.request_catch_up(peer);
                        }
                    }
                    Body::Forward { batch } => self.on_forward(from, &batch),
                    // Arriving at all is the point (`heard_from`, `watch.rs`).
                    Body::ForwardAck => {}
                    Body::Compacted { through } => self.on_compacted(from, through),
                    Body::ReadQuery { incarnation, round } => self.on_read_query(from, incarnation, round),
                    Body::ReadReply { incarnation, round, high } => {
                        self.on_read_reply(from, incarnation, round, high)
                    }
                    Body::RttProbe { incarnation, sent, commit, table } => {
                        self.on_rtt_probe(from, incarnation, sent, commit, table)
                    }
                    Body::RttReply { incarnation, sent } => self.on_rtt_reply(from, incarnation, sent),
                    // Nothing references a data batch yet (#172), so none is stored or served.
                    Body::Data { .. } | Body::Fetch { .. } => self.metrics.messages_rejected += 1,
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
        #[cfg(feature = "model")]
        self.probe_entries();
    }
}
