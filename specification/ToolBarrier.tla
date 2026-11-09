---- MODULE ToolBarrier ----
\* Sources:
\*   js/core.mjs
\*
\* The runner observes tool calls in event order. A wait tool snapshots earlier
\* Python handlers. It may publish success only after those handlers have written
\* their results to the core's FIFO stdin; FIFO processing commits those results
\* before the wait result. Abort cancels the wait instead of reporting success.
\* Broken control: WaitEarly lets the wait publish while a prior result is absent.
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS Calls, WaitEarly
VARIABLES admitted, prior, completed, writes, wait, aborted
vars == <<admitted, prior, completed, writes, wait, aborted>>
Init == /\ admitted = {} /\ prior = {} /\ completed = {}
        /\ writes = <<>> /\ wait = "not_started" /\ aborted = FALSE
Admit(c) == /\ wait = "not_started" /\ c \in Calls \ admitted
            /\ admitted' = admitted \cup {c}
            /\ UNCHANGED <<prior, completed, writes, wait, aborted>>
StartWait == /\ wait = "not_started" /\ ~aborted
             /\ prior' = admitted /\ wait' = "waiting"
             /\ UNCHANGED <<admitted, completed, writes, aborted>>
Finish(c) == /\ c \in admitted \ completed /\ ~aborted
             /\ completed' = completed \cup {c}
             /\ writes' = Append(writes, c)
             /\ UNCHANGED <<admitted, prior, wait, aborted>>
WaitDone == /\ wait = "waiting" /\ ~aborted
            /\ (WaitEarly \/ prior \subseteq completed)
            /\ wait' = "done" /\ writes' = Append(writes, "wait")
            /\ UNCHANGED <<admitted, prior, completed, aborted>>
Abort == /\ ~aborted /\ aborted' = TRUE
         /\ UNCHANGED <<admitted, prior, completed, writes, wait>>
Next == (\E c \in Calls : Admit(c) \/ Finish(c)) \/ StartWait \/ WaitDone \/ Abort
Spec == Init /\ [][Next]_vars
WaitHasPriorResults == IF wait = "done" THEN prior \subseteq completed ELSE TRUE
====
