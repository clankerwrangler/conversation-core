------------------------- MODULE BudgetAnchor -------------------------
\* Sources:
\*   src/process.rs
\*   src/budget.rs
\*   src/store.rs
EXTENDS TLC
CONSTANT Fault
VARIABLES
  phase,
  anchor,
  retained,
  overBudget,
  provedOverBudget,
  full,
  contiguous,
  reviewEdited,
  replyProof,
  measured,
  sawtooth

vars == <<
  phase, anchor, retained, overBudget, provedOverBudget,
  full, contiguous, reviewEdited, replyProof, measured, sawtooth
>>

Init ==
  /\ phase = "idle"
  /\ anchor = "none"
  /\ retained = "none"
  /\ overBudget = FALSE
  /\ provedOverBudget = FALSE
  /\ full = TRUE
  /\ contiguous = TRUE
  /\ reviewEdited = FALSE
  /\ replyProof = FALSE
  /\ measured = FALSE
  /\ sawtooth = FALSE

\* Canonical context grows beyond the model window (while chars/4 may still underestimate it).
GrowOverBudget ==
  /\ phase = "idle"
  /\ ~overBudget
  /\ overBudget' = TRUE
  /\ UNCHANGED <<phase, anchor, retained, provedOverBudget, full, contiguous,
                 reviewEdited, replyProof, measured, sawtooth>>

\* A new request consumes the previous anchor. A failed or aborted request cannot restore it.
\* A proved anchor is held as `retained` so a valid shortened canonical request can keep it.
Begin ==
  /\ phase = "idle"
  /\ phase' = "preparing"
  /\ anchor' = "none"
  /\ retained' = IF anchor = "proved" THEN "proved" ELSE "none"
  /\ full' = TRUE
  /\ contiguous' = TRUE
  /\ reviewEdited' = FALSE
  /\ replyProof' = FALSE
  /\ measured' = (anchor = "proved")
  /\ sawtooth' = FALSE
  /\ UNCHANGED <<overBudget, provedOverBudget>>

\* Guard shortening or runner context/body/reasoning substitution.
\* When overBudget and measured, the guard deterministically shortens (full' = FALSE).
\* When overBudget and unmeasured, chars/4 may underestimate and send full (full' = TRUE),
\* which causes a sawtooth bounce if overBudget was already proved on this unchanged context.
Select ==
  /\ phase = "preparing"
  /\ phase' = "sending"
  /\ reviewEdited' \in BOOLEAN
  /\ full' \in IF overBudget /\ measured THEN {FALSE} ELSE BOOLEAN
  /\ retained' = IF reviewEdited' THEN "none" ELSE retained
  /\ provedOverBudget' = IF reviewEdited' THEN FALSE ELSE provedOverBudget
  /\ sawtooth' = (provedOverBudget /\ ~reviewEdited' /\ full')
  /\ UNCHANGED <<anchor, overBudget, contiguous, replyProof, measured>>

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
  /\ UNCHANGED <<phase, anchor, retained, overBudget, provedOverBudget,
                 full, reviewEdited, replyProof, measured, sawtooth>>

\* Opening the reply locks its index in the canonical model view.
StartReply ==
  /\ phase = "sending"
  /\ phase' = "reply"
  /\ replyProof' = (full /\ contiguous /\ ~reviewEdited)
  /\ UNCHANGED <<anchor, retained, overBudget, provedOverBudget,
                 full, contiguous, reviewEdited, measured, sawtooth>>

\* A tool result written after an open reply starts lands after the reply in model view
\* and will be estimated in the suffix; it does not invalidate the opened prefix proof.
ModelWriteAfterReplyStart ==
  /\ phase = "reply"
  /\ UNCHANGED vars

Finish ==
  /\ phase = "reply"
  /\ phase' = "idle"
  /\ retained' = "none"
  /\ sawtooth' = FALSE
  /\ anchor' =
       IF replyProof THEN "proved"
       ELSE IF Fault = "TrustUnproved" THEN "unproved"
       ELSE IF Fault # "DropOnShortened" /\ ~full /\ ~reviewEdited /\ retained = "proved" THEN "proved"
       ELSE "none"
  /\ provedOverBudget' =
       IF replyProof /\ overBudget THEN TRUE
       ELSE IF Fault = "DropOnShortened" /\ ~full /\ ~reviewEdited /\ retained = "proved" THEN provedOverBudget
       ELSE IF anchor' = "proved" THEN provedOverBudget
       ELSE FALSE
  /\ UNCHANGED <<overBudget, full, contiguous, reviewEdited, replyProof, measured>>

\* Checkpoint, edited context file, abort, error, target change, or reopen clears any anchor.
Reset ==
  /\ phase \in {"idle", "preparing", "sending", "reply"}
  /\ phase' = "idle"
  /\ anchor' = "none"
  /\ retained' = "none"
  /\ provedOverBudget' = FALSE
  /\ measured' = FALSE
  /\ sawtooth' = FALSE
  /\ UNCHANGED <<overBudget, full, contiguous, reviewEdited, replyProof>>

Next ==
  \/ GrowOverBudget
  \/ Begin
  \/ Select
  \/ NonModelWrite
  \/ ModelWriteBeforeReply
  \/ StartReply
  \/ ModelWriteAfterReplyStart
  \/ Finish
  \/ Reset

NoUnprovedAnchor == anchor # "unproved"
ExactCoverage == anchor = "proved" => phase = "idle" /\ (replyProof \/ (~full /\ ~reviewEdited))
NeverMeasureUnproved == measured => anchor # "unproved"
NoOverBudgetSawtooth == ~sawtooth

Spec == Init /\ [][Next]_vars
=============================================================================
