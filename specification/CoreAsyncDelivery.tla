---- MODULE CoreAsyncDelivery ----
\* Sources:
\*   src/process.rs

\* One asynchronous result may arrive before or after a request captures its
\* context, or after the turn settles. A saved result not in a captured request
\* must wake a request; abort suppresses wake and cancels unfinished work.
EXTENDS Naturals
CONSTANT LoseBusyResult
VARIABLES phase, result, pending, visible, deliveries
vars == <<phase, result, pending, visible, deliveries>>
Init == /\ phase = "capture" /\ result = "running" /\ pending = FALSE
        /\ visible = FALSE /\ deliveries = 0
Finish == /\ result = "running" /\ phase # "aborted"
          /\ result' = "done"
          /\ pending' = IF LoseBusyResult /\ phase = "stream" THEN FALSE ELSE TRUE
          /\ UNCHANGED <<phase, visible, deliveries>>
Capture == /\ phase = "capture"
           /\ phase' = "stream" /\ visible' = (visible \/ pending)
           /\ deliveries' = deliveries + (IF pending THEN 1 ELSE 0)
           /\ pending' = FALSE /\ UNCHANGED result
End == /\ phase = "stream"
       /\ phase' = IF pending THEN "capture" ELSE "idle"
       /\ UNCHANGED <<result, pending, visible, deliveries>>
Wake == /\ phase = "idle" /\ pending /\ phase' = "capture"
        /\ UNCHANGED <<result, pending, visible, deliveries>>
Abort == /\ phase \in {"capture", "stream"}
         /\ phase' = "aborted" /\ pending' = FALSE
         /\ result' = IF result = "running" THEN "cancelled" ELSE result
         /\ UNCHANGED <<visible, deliveries>>
Next == Finish \/ Capture \/ End \/ Wake \/ Abort
Spec == Init /\ [][Next]_vars /\ WF_vars(Capture) /\ WF_vars(End) /\ WF_vars(Wake)
EventuallyVisible == [](result = "done" => <> (visible \/ phase = "aborted"))
AtMostOneDelivery == deliveries <= 1
====
