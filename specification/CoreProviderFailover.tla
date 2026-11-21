---- MODULE CoreProviderFailover ----
\* Sources:
\*   src/process.rs
\*   src/provider.rs

\* Opted-in turn. A clean rejection can be retried; a proven outage can
\* switch providers once. Any output (including tool dispatch) seals the turn.
\* Preparing the fallback is interruptible before dispatch. A fresh turn uses
\* the primary again; a follow-up request inside a turn keeps its route and seal.
EXTENDS Naturals
CONSTANT BypassFence
VARIABLES phase, route, exposed, switches, unsafe
vars == <<phase, route, exposed, switches, unsafe>>
Init == /\ phase = "idle" /\ route = "primary" /\ exposed = FALSE
        /\ switches = 0 /\ unsafe = FALSE
Start == /\ phase \in {"idle", "ended"}
         /\ phase' = "stream" /\ route' = "primary" /\ exposed' = FALSE
         /\ switches' = 0 /\ UNCHANGED unsafe
Expose == /\ phase = "stream" /\ exposed' = TRUE
          /\ UNCHANGED <<phase, route, switches, unsafe>>
Outage == /\ phase = "stream" /\ phase' = "outage"
          /\ UNCHANGED <<route, exposed, switches, unsafe>>
Prepare == /\ phase = "outage" /\ route = "primary"
           /\ (~exposed \/ BypassFence)
           /\ phase' = "prepare" /\ UNCHANGED <<route, exposed, switches, unsafe>>
Dispatch == /\ phase = "prepare"
            /\ phase' = "stream" /\ route' = "fallback"
            /\ switches' = switches + 1 /\ unsafe' = (unsafe \/ exposed)
            /\ UNCHANGED exposed
End == /\ phase \in {"stream", "outage", "prepare"}
       /\ phase' = "ended" /\ UNCHANGED <<route, exposed, switches, unsafe>>
Next == Start \/ Expose \/ Outage \/ Prepare \/ Dispatch \/ End
Spec == Init /\ [][Next]_vars
Safe == ~unsafe /\ switches <= 1
====
