---- MODULE ReaperReferences ----
\* A cleanup (`core reap`, a process separate from the running core) deletes old stored files, skipping those that
\* `core referenced` lists. Meanwhile the core keeps running: the model can edit its context
\* file to refer to any stored file that exists (the core rejects references to missing files),
\* or drop references. Every request resolves the context's references.
\*
\* Both take one lock per session (references.lock): the reaper from its check until it is done
\* deleting, the core only while accepting a new context. The operating system releases the
\* reaper's lock when it stops, even if it crashes halfway.
\*
\* Safety: no request ever needs a file that was deleted.
\* Broken control (NoLock): without the lock, a context edit between the reaper's check and
\* its deletion refers to a file that is then deleted.
EXTENDS FiniteSets

CONSTANTS Files, NoLock

VARIABLES
    stored,    \* files that exist
    refs,      \* files the current context refers to
    listed,    \* the reaper's copy of `core referenced`
    reaper,    \* "idle" | "checked"
    lock,      \* "free" | "reaper"
    failed     \* a request needed a missing file

vars == <<stored, refs, listed, reaper, lock, failed>>

Init ==
    /\ stored = Files
    /\ refs = {}
    /\ listed = {}
    /\ reaper = "idle"
    /\ lock = "free"
    /\ failed = FALSE

\* The core accepts an edit (or checkpoint) referring to a stored file, holding the lock
\* for the moment of acceptance.
AddRef(f) ==
    /\ NoLock \/ lock = "free"
    /\ f \in stored
    /\ refs' = refs \cup {f}
    /\ UNCHANGED <<stored, listed, reaper, lock, failed>>

DropRef(f) ==
    /\ f \in refs
    /\ refs' = refs \ {f}
    /\ UNCHANGED <<stored, listed, reaper, lock, failed>>

Request ==
    /\ failed' = ~(refs \subseteq stored)
    /\ UNCHANGED <<stored, refs, listed, reaper, lock>>

\* The reaper takes the lock, then asks `core referenced`.
Check ==
    /\ reaper = "idle"
    /\ NoLock \/ lock = "free"
    /\ lock' = IF NoLock THEN lock ELSE "reaper"
    /\ listed' = refs
    /\ reaper' = "checked"
    /\ UNCHANGED <<stored, refs, failed>>

Delete(f) ==
    /\ reaper = "checked"
    /\ f \in stored \ listed
    /\ stored' = stored \ {f}
    /\ UNCHANGED <<refs, listed, reaper, lock, failed>>

\* Finishing, or crashing at any point after the check: either way the lock is released.
Stop ==
    /\ reaper = "checked"
    /\ reaper' = "idle"
    /\ lock' = "free"
    /\ UNCHANGED <<stored, refs, listed, failed>>

Next ==
    \/ \E f \in Files : AddRef(f) \/ DropRef(f) \/ Delete(f)
    \/ Request \/ Check \/ Stop

Spec == Init /\ [][Next]_vars

NoMissingData == ~failed
====
