---- MODULE CoreWriterLock ----
\* Core processes started on the same session. Each appends records numbered one past the
\* tail it read. With the lock (writer.lock), a core takes the lock before reading the tail,
\* without waiting: if another core holds it, the new one stops at once. The operating
\* system releases the lock when a core ends, also when it crashes.
\*
\* Safety: no two records share a number.
\* Broken control (NoLock): two cores read the same tail and both append its successor.
EXTENDS Naturals, FiniteSets

CONSTANTS Cores, MaxRecords, NoLock

VARIABLES
    log,      \* the numbers appended so far, as a set of <<number, core>> pairs
    state,    \* per core: "off" | "running" | "read" | "stopped"
    read,     \* per core: the tail it read (the next number it will use is read + 1)
    lock      \* the core holding the lock, or "free"

vars == <<log, state, read, lock>>

Tail == IF log = {} THEN 0 ELSE CHOOSE n \in {r[1] : r \in log} : \A r \in log : r[1] <= n

Init ==
    /\ log = {}
    /\ state = [c \in Cores |-> "off"]
    /\ read = [c \in Cores |-> 0]
    /\ lock = "free"

\* A core starts: it takes the lock without waiting, or stops if another holds it.
Start(c) ==
    /\ state[c] = "off"
    /\ IF NoLock \/ lock = "free"
       THEN /\ state' = [state EXCEPT ![c] = "running"]
            /\ lock' = IF NoLock THEN lock ELSE c
       ELSE /\ state' = [state EXCEPT ![c] = "stopped"]
            /\ UNCHANGED lock
    /\ UNCHANGED <<log, read>>

\* Appending is two steps: read the tail, then write the next number.
ReadTail(c) ==
    /\ state[c] = "running"
    /\ read' = [read EXCEPT ![c] = Tail]
    /\ state' = [state EXCEPT ![c] = "read"]
    /\ UNCHANGED <<log, lock>>

Append(c) ==
    /\ state[c] = "read"
    /\ Cardinality(log) < MaxRecords
    /\ log' = log \cup {<<read[c] + 1, c>>}
    /\ state' = [state EXCEPT ![c] = "running"]
    /\ UNCHANGED <<read, lock>>

\* A core ends, closing or crashing; the operating system releases its lock.
End(c) ==
    /\ state[c] \in {"running", "read"}
    /\ state' = [state EXCEPT ![c] = "off"]
    /\ lock' = IF lock = c THEN "free" ELSE lock
    /\ UNCHANGED <<log, read>>

Next == \E c \in Cores : Start(c) \/ ReadTail(c) \/ Append(c) \/ End(c)

Spec == Init /\ [][Next]_vars

Bounded == Cardinality(log) <= MaxRecords

UniqueNumbers == \A r1, r2 \in log : r1[1] = r2[1] => r1 = r2
====
