---- MODULE CoreToolHandoff ----
\* Sources:
\*   src/process.rs

\* One reply with tool calls, in the conversation core. A call is saved as soon as it
\* is complete in the stream, then started; results arrive from the runner whenever
\* the call finishes, also while the reply is still streaming. Calls are normal or
\* async (async results may arrive after the next request).
\*
\* The connection can drop mid-reply: with nothing saved the request is retried
\* from scratch (new attempt); otherwise the saved part stays, saved calls run to
\* completion, a cut-off note is saved, and the conversation continues. An abort
\* stops the reply and cancels every unfinished call. A crash loses the process
\* (and any result not yet saved); on restart every saved call without a result
\* gets an interrupted result. Nothing is ever re-run.
\*
\* Broken controls: StartBeforeSave starts a call before it is saved; SendEarly sends
\* the next request without waiting for normal results; RetryAfterSave retries from
\* scratch although a call was already saved.

EXTENDS Naturals, FiniteSets

CONSTANTS Attempts, Idx, Async, MaxCrashes, StartBeforeSave, SendEarly, RetryAfterSave

Calls == Attempts \X Idx
IsAsync(c) == c[2] \in Async

VARIABLES
  attempt,     \* current request attempt
  stream,      \* "streaming" | "ended" | "dropped" | "aborted" | "none"
  received,    \* calls complete in the stream (not durable)
  saved,       \* calls saved in the log (durable)
  started,     \* calls whose effect has begun (real world, survives crashes)
  running,     \* calls the runner is executing for this process
  result,      \* durable result per call: "none" | "done" | "cancelled" | "interrupted"
  note,        \* cut-off note saved
  pc,          \* "reply" | "drain" | "sent" | "recover"
  crashes,
  badRequest   \* ghost: a request was sent missing a normal call's result

vars == <<attempt, stream, received, saved, started, running, result, note, pc, crashes, badRequest>>

Init ==
  /\ attempt = 1
  /\ stream = "streaming"
  /\ received = {} /\ saved = {} /\ started = {} /\ running = {}
  /\ result = [c \in Calls |-> "none"]
  /\ note = FALSE
  /\ pc = "reply"
  /\ crashes = 0
  /\ badRequest = FALSE

ThisAttempt == {attempt} \X Idx

CallComplete(c) ==
  /\ pc = "reply" /\ stream = "streaming"
  /\ c \in ThisAttempt /\ c \notin received
  /\ received' = received \cup {c}
  /\ UNCHANGED <<attempt, stream, saved, started, running, result, note, pc, crashes, badRequest>>

SaveCall(c) ==
  /\ pc \in {"reply", "drain"}
  /\ c \in received /\ c \notin saved
  /\ saved' = saved \cup {c}
  /\ UNCHANGED <<attempt, stream, received, started, running, result, note, pc, crashes, badRequest>>

StartCall(c) ==
  /\ pc \in {"reply", "drain", "sent"}
  /\ c \notin started
  /\ result[c] = "none"
  /\ IF StartBeforeSave THEN c \in received ELSE c \in saved
  /\ started' = started \cup {c}
  /\ running' = running \cup {c}
  /\ UNCHANGED <<attempt, stream, received, saved, result, note, pc, crashes, badRequest>>

\* The runner reports a finished call; the core saves the result.
Finish(c) ==
  /\ pc # "recover"
  /\ c \in running
  /\ c \in saved
  /\ running' = running \ {c}
  /\ result' = [result EXCEPT ![c] = "done"]
  /\ UNCHANGED <<attempt, stream, received, saved, started, note, pc, crashes, badRequest>>

StreamEnds ==
  /\ pc = "reply" /\ stream = "streaming"
  /\ received = saved  \* every complete call is saved before the end is handled
  /\ stream' = "ended" /\ pc' = "drain"
  /\ UNCHANGED <<attempt, received, saved, started, running, result, note, crashes, badRequest>>

Drop ==
  /\ pc = "reply" /\ stream = "streaming"
  /\ stream' = "dropped"
  /\ UNCHANGED <<attempt, received, saved, started, running, result, note, pc, crashes, badRequest>>

\* Nothing saved: retry the request from scratch as a new attempt.
Retry ==
  /\ pc = "reply" /\ stream = "dropped"
  /\ attempt < Cardinality(Attempts)
  /\ IF RetryAfterSave THEN TRUE ELSE saved \cap ThisAttempt = {}
  /\ attempt' = attempt + 1
  /\ stream' = "streaming"
  /\ received' = {}
  /\ UNCHANGED <<saved, started, running, result, note, pc, crashes, badRequest>>

\* Something saved: keep it, add the cut-off note, and drain like an ended reply.
CutOff ==
  /\ pc = "reply" /\ stream = "dropped"
  /\ saved \cap ThisAttempt # {} \/ attempt = Cardinality(Attempts)
  /\ note' = TRUE
  /\ pc' = "drain"
  /\ received' = saved
  /\ UNCHANGED <<attempt, stream, saved, started, running, result, crashes, badRequest>>

Abort ==
  /\ pc \in {"reply", "drain", "sent"}
  /\ stream' = "aborted"
  /\ result' = [c \in Calls |-> IF c \in saved /\ result[c] = "none" THEN "cancelled" ELSE result[c]]
  /\ running' = {}
  /\ pc' = "sent"
  /\ UNCHANGED <<attempt, received, saved, started, note, crashes, badRequest>>

\* Next request: every saved normal call has its result (async may still be pending).
NormalDone == \A c \in saved : IsAsync(c) \/ result[c] # "none"
Send ==
  /\ pc = "drain"
  /\ saved \subseteq started \/ StartBeforeSave
  /\ IF SendEarly THEN TRUE ELSE NormalDone
  /\ badRequest' = (badRequest \/ ~NormalDone)
  /\ pc' = "sent"
  /\ UNCHANGED <<attempt, stream, received, saved, started, running, result, note, crashes>>

Crash ==
  /\ pc # "recover" /\ crashes < MaxCrashes
  /\ crashes' = crashes + 1
  /\ pc' = "recover"
  /\ running' = {}
  /\ received' = {}
  /\ UNCHANGED <<attempt, stream, saved, started, result, note, badRequest>>

\* On open: every saved call without a result gets an interrupted result. Never re-run.
Recover ==
  /\ pc = "recover"
  /\ result' = [c \in Calls |-> IF c \in saved /\ result[c] = "none" THEN "interrupted" ELSE result[c]]
  /\ stream' = "none"
  /\ pc' = "sent"
  /\ UNCHANGED <<attempt, received, saved, started, running, note, crashes, badRequest>>

Next ==
  \/ \E c \in Calls : CallComplete(c) \/ SaveCall(c) \/ StartCall(c) \/ Finish(c)
  \/ StreamEnds \/ Drop \/ Retry \/ CutOff \/ Abort \/ Send \/ Crash \/ Recover

Spec == Init /\ [][Next]_vars

\* A call's effect never begins without the call saved.
NoEffectWithoutRecord == started \subseteq saved

\* A request never goes out missing a normal call's result.
NoIncompleteRequest == ~badRequest

\* A call that already has a result is never running again (nothing runs twice).
NoRunAfterResult == \A c \in running : result[c] = "none"

\* A retry never leaves a saved call behind from an earlier attempt.
NoAbandonedCall == \A c \in saved : c[1] = attempt

\* Every started call that is no longer running has exactly one recorded outcome,
\* unless the process is between a crash and its recovery.
StartedCallsSettle == pc # "recover" => \A c \in started \ running : result[c] # "none"
====
