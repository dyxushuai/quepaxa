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
            commit_index: self.held,
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
        // Anyone may read, disseminate or fetch: a joining or removed replica exchanges
        // these with members it may not know yet.
        let request = matches!(
            msg.body,
            Body::RecordRequest { .. }
                | Body::CatchUp { .. }
                | Body::Forward { .. }
                | Body::ReadQuery { .. }
                | Body::Data { .. }
                | Body::Fetch { .. }
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
        let outside = self.outside();
        self.heard_from(from);
        self.watch.committed.insert(from, msg.env.commit_index);
        if self.tuner.is_some()
            && self.leads_now()
            && !matches!(
                msg.body,
                Body::ForwardAck { .. }
                    | Body::RttProbe { .. }
                    | Body::RttReply { .. }
                    | Body::CatchUp { .. }
                    | Body::Entries { .. }
            )
            && self.schedule.members(Slot(self.committed)).contains(&from)
        {
            let silence_limit = self
                .request_rtt
                .unwrap_or(self.cfg.retransmit_interval)
                .saturating_mul(4)
                .max(self.cfg.retransmit_interval.saturating_mul(2));
            if let Some(w) = self.tuner.as_mut().and_then(|t| {
                t.on_peer_commit(from, msg.env.commit_index, self.committed, self.now, silence_limit)
            }) {
                self.apply_tuned_w(w);
            }
        }
        let (is_entries, non_empty_entries) = match &msg.body {
            Body::Entries { proposals, .. } => (true, !proposals.is_empty()),
            _ => (false, false),
        };
        let advanced = non_empty_entries || msg.env.commit_index > self.committed;
        self.note_follow_heard(from, is_entries, advanced);
        // Control/liveness messages may travel on a separate stream ahead of `Decided`; do not
        // let their envelope trigger speculative bulk catch-up on voting members before
        // in-flight decisions land. `Data` also travels on the bulk stream, so record its
        // `commit_index` for timer-driven recovery without firing an immediate `CatchUp`.
        let control =
            matches!(msg.body, Body::ForwardAck { .. } | Body::RttProbe { .. } | Body::RttReply { .. });
        if outside || !control {
            self.catch_up.heard(from, msg.env.commit_index);
            if !outside && self.catch_up.ahead_of(self.committed) {
                self.seen.entry(Slot(self.committed)).or_insert(self.now);
            }
            let lag = self.catch_up_lag();
            // `Entries` is handled after applying its page in `on_entries`, so an observer's
            // non-empty follow reply does not echo a stale duplicate `CatchUp` before advancing.
            if !matches!(msg.body, Body::Compacted { .. } | Body::Entries { .. })
                && (outside || !matches!(msg.body, Body::Data { .. }))
                && msg.env.commit_index > self.committed.saturating_add(lag)
                && self.catch_up_due()
            {
                self.request_catch_up(from);
            }
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
                    Body::Entries { first, proposals, content } => {
                        self.on_entries(from, first, proposals, content)
                    }
                    Body::Forward { seq, batch } => self.on_forward(from, seq, &batch),
                    // Arriving at all shows the sender alive (`heard_from`, `watch.rs`); the
                    // numbers say which forwards it took (`forwards.rs`).
                    Body::ForwardAck { seqs } => self.watch.fwd.acked(from, &seqs, self.now),
                    Body::Compacted { through } => self.on_compacted(from, through),
                    Body::ReadQuery { incarnation, round } => self.on_read_query(from, incarnation, round),
                    Body::ReadReply { incarnation, round, high } => {
                        self.on_read_reply(from, incarnation, round, high)
                    }
                    Body::RttProbe { incarnation, sent, commit, table } => {
                        self.on_rtt_probe(from, incarnation, sent, commit, table)
                    }
                    Body::RttReply { incarnation, sent } => self.on_rtt_reply(from, incarnation, sent),
                    Body::Data { id, bytes } => self.on_data(from, id, bytes),
                    Body::Fetch { slot, ids } => self.on_fetch(from, slot, ids),
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
