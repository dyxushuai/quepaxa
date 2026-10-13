# What must persist, and how does a replica recover from crash-restart

Parent: [map](../map.md)
Type: grilling
Status: open
Blocked by: 02, 04

## Question

The paper assumes crash-stop. Decide the crash-restart model: exactly which recorder/proposer state is written durably before a reply is sent, what a restarted replica may and may not do before rejoining (e.g. reusing priorities or proposer state), how decided slots are learned/broadcast and caught up, and which of the paper's ambiguities A1–A16 (see the paper protocol spec answer) the design must close and how.

Note (from the phase-one scope answer): reconfiguration is not in v1, so "amnesia ⇒ rejoin as a new member" is not available as the v1 recovery strategy.
