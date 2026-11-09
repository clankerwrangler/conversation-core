---- MODULE CoreCallDedup ----
\* Sources:
\*   src/process.rs

\* Completed provider items can repeat before or after a result, or after an
\* abort and a later request. The call map owns the dispatch decision. Counts
\* are ghost state used only to check externally observable effects/records.
EXTENDS Naturals, FiniteSets
CONSTANTS Calls, SkipGuard
VARIABLES state, dispatches, records, cancelled
vars == <<state, dispatches, records, cancelled>>
Init == /\ state = [c \in Calls |-> "absent"]
        /\ dispatches = [c \in Calls |-> 0]
        /\ records = [c \in Calls |-> 0] /\ cancelled = {}
Item(c) == /\ c \in Calls /\ (state[c] = "absent" \/ SkipGuard)
           /\ state' = [state EXCEPT ![c] = "running"]
           /\ dispatches' = [dispatches EXCEPT ![c] = @ + 1]
           /\ records' = [records EXCEPT ![c] = @ + 1]
           /\ UNCHANGED cancelled
Result(c) == /\ c \in Calls /\ state[c] = "running"
             /\ state' = [state EXCEPT ![c] = "done"]
             /\ UNCHANGED <<dispatches, records, cancelled>>
Abort == /\ \E c \in Calls : state[c] = "running"
         /\ cancelled' = cancelled \cup {c \in Calls : state[c] = "running"}
         /\ state' = [c \in Calls |-> IF state[c] = "running" THEN "cancelled" ELSE state[c]]
         /\ UNCHANGED <<dispatches, records>>
Next == (\E c \in Calls : Item(c) \/ Result(c)) \/ Abort
Spec == Init /\ [][Next]_vars
NoRepeatEffect == \A c \in Calls : dispatches[c] <= 1
NoDuplicateCallRecord == \A c \in Calls : records[c] <= 1
CancelledNeverRestarts == \A c \in cancelled : state[c] # "running"
====
