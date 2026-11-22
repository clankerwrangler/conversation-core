------------------------- MODULE BudgetAnchor -------------------------
\* Sources:
\*   src/process.rs
\*   src/budget.rs
\*   src/store.rs
EXTENDS TLC
CONSTANT Fault
VARIABLES phase, anchor, full, contiguous, replyProof, measured
vars == <<phase, anchor, full, contiguous, replyProof, measured>>

Init ==
  /\ phase = "idle"
  /\ anchor = "none"
  /\ full = TRUE
  /\ contiguous = TRUE
  /\ replyProof = FALSE
  /\ measured = FALSE

\* A new request consumes the previous anchor. A failed or aborted request cannot restore it.
Begin ==
  /\ phase = "idle"
  /\ phase' = "preparing"
  /\ anchor' = "none"
  /\ full' = TRUE
  /\ contiguous' = TRUE
  /\ replyProof' = FALSE
  /\ measured' \in IF anchor = "proved" THEN {TRUE, FALSE} ELSE {FALSE}

\* Guard shortening or runner context/body/reasoning substitution leaves prefix coverage unproved.
Select ==
  /\ phase = "preparing"
  /\ phase' = "sending"
  /\ full' \in BOOLEAN
  /\ UNCHANGED <<anchor, contiguous, replyProof, measured>>

\* Non-model bookkeeping (app, settings) does not alter the model-visible prefix.
NonModelWrite ==
  /\ phase \in {"preparing", "sending", "reply"}
  /\ UNCHANGED vars

\* A model-visible record appended between snapshot and reply start shifts the reply's
\* prefix, so the provider never saw that record.
ModelWriteBeforeReply ==
  /\ phase \in {"preparing", "sending"}
  /\ contiguous
  /\ contiguous' = FALSE
  /\ UNCHANGED <<phase, anchor, full, replyProof, measured>>

\* Opening the reply locks its index in the canonical model view.
StartReply ==
  /\ phase = "sending"
  /\ phase' = "reply"
  /\ replyProof' = (full /\ contiguous)
  /\ UNCHANGED <<anchor, full, contiguous, measured>>

\* A tool result written after an open reply starts lands after the reply in model view
\* and will be estimated in the suffix; it does not invalidate the opened prefix proof.
ModelWriteAfterReplyStart ==
  /\ phase = "reply"
  /\ UNCHANGED vars

Finish ==
  /\ phase = "reply"
  /\ phase' = "idle"
  /\ anchor' = IF replyProof THEN "proved" ELSE IF Fault THEN "unproved" ELSE "none"
  /\ UNCHANGED <<full, contiguous, replyProof, measured>>

\* Checkpoint, edited context file, abort, error, or reopen clears any anchor.
Reset ==
  /\ phase \in {"idle", "preparing", "sending", "reply"}
  /\ phase' = "idle"
  /\ anchor' = "none"
  /\ measured' = FALSE
  /\ UNCHANGED <<full, contiguous, replyProof>>

Next ==
  \/ Begin
  \/ Select
  \/ NonModelWrite
  \/ ModelWriteBeforeReply
  \/ StartReply
  \/ ModelWriteAfterReplyStart
  \/ Finish
  \/ Reset

NoUnprovedAnchor == anchor # "unproved"
ExactCoverage == anchor = "proved" => phase = "idle" /\ replyProof /\ full /\ contiguous
NeverMeasureUnproved == measured => anchor # "unproved"

Spec == Init /\ [][Next]_vars
=============================================================================
