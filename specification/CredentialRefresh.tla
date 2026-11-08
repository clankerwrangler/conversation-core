---- MODULE CredentialRefresh ----
\* Sessions share one credentials file. Providers rotate refresh tokens: a renewal returns a
\* new refresh token and the old one stops working. A session that finds its token about to
\* expire renews it.
\*
\* With the lock (auth.json.lock), a session takes the lock, re-reads the file, renews only if
\* the file still holds the token it found expiring, writes the result and releases the lock.
\* A session that waited sees the newer token and does not renew again. The operating system
\* releases the lock when a process dies.
\*
\* Safety: the provider never refuses a refresh token (no session is signed out by another).
\* Broken control (NoLock): two sessions read the same refresh token and both send it; the
\* second is refused.
EXTENDS Naturals

CONSTANTS Sessions, MaxVersion, NoLock

VARIABLES
    server,   \* the refresh token version the provider accepts
    file,     \* the version stored in the file
    seen,     \* per session: the version it found expiring (0 = not renewing)
    state,    \* per session: "idle" | "want" | "locked"
    lock,     \* the session holding the lock, or "free"
    refused   \* the provider refused a refresh token

vars == <<server, file, seen, state, lock, refused>>

Init ==
    /\ server = 1
    /\ file = 1
    /\ seen = [s \in Sessions |-> 0]
    /\ state = [s \in Sessions |-> "idle"]
    /\ lock = "free"
    /\ refused = FALSE

\* A session reads the file and finds its token about to expire.
Notice(s) ==
    /\ state[s] = "idle"
    /\ seen' = [seen EXCEPT ![s] = file]
    /\ state' = [state EXCEPT ![s] = "want"]
    /\ UNCHANGED <<server, file, lock, refused>>

\* Send refresh token `v`: the provider accepts only its current one, and rotates it.
Send(v) ==
    IF v = server
    THEN /\ server' = server + 1
         /\ file' = server + 1
         /\ refused' = refused
    ELSE /\ refused' = TRUE
         /\ UNCHANGED <<server, file>>

Lock(s) ==
    /\ ~NoLock
    /\ state[s] = "want"
    /\ lock = "free"
    /\ lock' = s
    /\ state' = [state EXCEPT ![s] = "locked"]
    /\ UNCHANGED <<server, file, seen, refused>>

\* Holding the lock: renew only if the file still holds the token found expiring.
Renew(s) ==
    /\ state[s] = "locked"
    /\ IF file = seen[s] THEN Send(file) ELSE UNCHANGED <<server, file, refused>>
    /\ lock' = "free"
    /\ state' = [state EXCEPT ![s] = "idle"]
    /\ seen' = [seen EXCEPT ![s] = 0]

\* Without the lock: send the token that was found expiring.
RenewUnlocked(s) ==
    /\ NoLock
    /\ state[s] = "want"
    /\ Send(seen[s])
    /\ state' = [state EXCEPT ![s] = "idle"]
    /\ seen' = [seen EXCEPT ![s] = 0]
    /\ UNCHANGED lock

Next == \E s \in Sessions : Notice(s) \/ Lock(s) \/ Renew(s) \/ RenewUnlocked(s)

Spec == Init /\ [][Next]_vars

Bounded == server <= MaxVersion

NeverRefused == ~refused
====
