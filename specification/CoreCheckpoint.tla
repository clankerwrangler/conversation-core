---- MODULE CoreCheckpoint ----
\* Sources:
\*   src/store.rs

\* Writing a checkpoint in the conversation core, and sealing the chunk after it.
\* Messages (including late tool results) can be appended at any moment, and the
\* process can crash after any step. Each step is one durable write.
\*
\* The context rebuilt from disk is: the newest checkpoint marker's content, then
\* every message whose sequence number is above that marker's `through`. It must
\* always equal the conversation's meaning (`logical`), including straight after a
\* crash, before recovery runs.
\*
\* Steps: write content -> append marker -> prune all but the newest two
\* checkpoint files -> append the next chunk's first record. Recovery deletes
\* content files no marker names and finishes pruning and sealing.
\*
\* Broken controls: MarkerFirst appends the marker before the content exists;
\* PruneEarly keeps only the new file before its marker is written.

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS MaxMsgs, MaxCps, MaxCrashes, MarkerFirst, PruneEarly

VARIABLES log, files, pc, cur, thr, msgs, cps, crashes, logical

vars == <<log, files, pc, cur, thr, msgs, cps, crashes, logical>>

Max(S) == CHOOSE x \in S : \A y \in S : y <= x

Msg(v) == [k |-> "msg", v |-> v]
Content(c) == [k |-> "content", id |-> c]

MarkerIdx == {i \in 1..Len(log) : log[i].k = "cp"}
NewestMarker == IF MarkerIdx = {} THEN 0 ELSE Max(MarkerIdx)
MarkerIds == {log[i].id : i \in MarkerIdx}
LastTwoIds ==
  LET idx == MarkerIdx
      top == IF idx = {} THEN {} ELSE {Max(idx)}
      rest == idx \ top
      second == IF rest = {} THEN {} ELSE {Max(rest)}
  IN {log[i].id : i \in top \cup second}

MsgsAbove(t) == SelectSeq([i \in 1..Len(log) |-> log[i]], LAMBDA r : r.k = "msg" /\ r.v > t)
AllMsgs == MsgsAbove(0)

\* What the core reads back from disk.
Context ==
  IF NewestMarker = 0 THEN AllMsgs
  ELSE LET m == log[NewestMarker] IN
       IF m.id \in files THEN <<Content(m.id)>> \o MsgsAbove(m.through)
       ELSE <<[k |-> "broken"]>>

Init ==
  /\ log = <<[k |-> "chunk"]>>
  /\ files = {}
  /\ pc = "idle"
  /\ cur = 0
  /\ thr = 0
  /\ msgs = 0
  /\ cps = 0
  /\ crashes = 0
  /\ logical = <<>>

\* A user message, reply or late tool result: any time, also mid-checkpoint.
AppendMsg ==
  /\ msgs < MaxMsgs
  /\ pc # "recover"
  /\ msgs' = msgs + 1
  /\ log' = Append(log, Msg(msgs + 1))
  /\ logical' = Append(logical, Msg(msgs + 1))
  /\ UNCHANGED <<files, pc, cur, thr, cps, crashes>>

\* The new checkpoint replaces everything up to `through` (the last record so far).
Begin ==
  /\ pc = "idle"
  /\ cps < MaxCps
  /\ cps' = cps + 1
  /\ cur' = cps + 1
  /\ thr' = msgs
  /\ pc' = IF MarkerFirst THEN "content-skipped" ELSE "content"
  /\ UNCHANGED <<log, files, msgs, crashes, logical>>

WriteContent ==
  /\ \/ /\ pc = "content"
        /\ pc' = "marker"
     \/ /\ pc = "late-content"
        /\ pc' = "prune"
  /\ files' = IF PruneEarly THEN {cur} ELSE files \cup {cur}
  /\ UNCHANGED <<log, cur, thr, msgs, cps, crashes, logical>>

Marker ==
  /\ \/ /\ pc = "marker"
        /\ pc' = "prune"
     \/ /\ pc = "content-skipped"
        /\ pc' = "late-content"
  /\ log' = Append(log, [k |-> "cp", id |-> cur, through |-> thr])
  \* The marker is when the checkpoint takes effect.
  /\ logical' = <<Content(cur)>> \o SelectSeq(logical, LAMBDA r : r.k = "msg" /\ r.v > thr)
  /\ UNCHANGED <<files, cur, thr, msgs, cps, crashes>>

Prune ==
  /\ pc = "prune"
  /\ files' = files \cap LastTwoIds
  /\ pc' = "seal"
  /\ UNCHANGED <<log, cur, thr, msgs, cps, crashes, logical>>

Seal ==
  /\ pc = "seal"
  /\ log' = Append(log, [k |-> "chunk"])
  /\ pc' = "idle"
  /\ UNCHANGED <<files, cur, thr, msgs, cps, crashes, logical>>

Crash ==
  /\ crashes < MaxCrashes
  /\ pc # "recover"
  /\ crashes' = crashes + 1
  /\ pc' = "recover"
  /\ UNCHANGED <<log, files, cur, thr, msgs, cps, logical>>

\* Sealed since the newest marker: a chunk record follows it.
SealedAfterNewest ==
  NewestMarker = 0 \/ \E i \in (NewestMarker + 1)..Len(log) : log[i].k = "chunk"

\* On open: keep only checkpoint files that are among the newest two markers
\* (drops content no marker names, and finishes pruning); seal if no chunk was
\* started since the newest marker. Messages may follow the marker, so "the log
\* ends with a marker" is not the test.
Recover ==
  /\ pc = "recover"
  /\ files' = files \cap LastTwoIds
  /\ log' = IF SealedAfterNewest THEN log ELSE Append(log, [k |-> "chunk"])
  /\ pc' = "idle"
  /\ UNCHANGED <<cur, thr, msgs, cps, crashes, logical>>

Next == AppendMsg \/ Begin \/ WriteContent \/ Marker \/ Prune \/ Seal \/ Crash \/ Recover

Spec == Init /\ [][Next]_vars

\* The context on disk always means what the conversation means.
ContextIsLogical == Context = logical

\* After recovery, every checkpoint file is one a marker names.
NoOrphansWhenIdle == pc = "idle" => files \subseteq MarkerIds

\* At most the newest two checkpoint files are kept once a write settles.
AtMostTwoKept == pc = "idle" => Cardinality(files) <= 2

\* Once a write settles, the chunk after the newest marker has been started.
SealedWhenIdle == pc = "idle" => SealedAfterNewest
====
