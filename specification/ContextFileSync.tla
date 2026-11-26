----------------------- MODULE ContextFileSync -----------------------
\* Sources:
\*   src/context_file.rs
\*   src/process.rs
\*   src/store.rs
\*
\* Models synchronization between Core (accept + publish of context.jsonl)
\* and a concurrent async tool call or external editor.
\*
\* Fault values:
\*   "None"         - Fixed protocol: skip accept & publish while any tool call
\*                    is running; run accept + publish together when quiescent
\*                    and refuse to overwrite if disk changed mid-sync.
\*   "NoQuiesce"    - Previous Core bug: runs accept and publish while an async
\*                    tool call is still executing, allowing publish to clobber
\*                    the tool's edit.
\*   "AcceptOnly"   - Broken half-fix: skips publish while a tool is running
\*                    but still runs accept, leaving state.json stale so the
\*                    same edit is accepted twice.
\*   "BlindPublish" - Broken publish: overwrites context.jsonl without checking
\*                    whether an external edit landed after accept read disk.

EXTENDS Naturals, TLC
CONSTANT Fault
VARIABLES
  corePhase,
  toolState,
  diskFile,
  stateCurrent,
  observedDisk,
  editWritten,
  acceptedCount,
  canonicalHasEdit,
  resultSaved,
  canonicalHasResult

vars == <<
  corePhase, toolState, diskFile, stateCurrent, observedDisk,
  editWritten, acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult
>>

Init ==
  /\ corePhase = "idle"
  /\ toolState = "none"
  /\ diskFile = "init"
  /\ stateCurrent = "init"
  /\ observedDisk = "none"
  /\ editWritten = FALSE
  /\ acceptedCount = 0
  /\ canonicalHasEdit = FALSE
  /\ resultSaved = FALSE
  /\ canonicalHasResult = FALSE

\* While any tool call is running, the fixed protocol freezes context.jsonl
\* (skips both accept and publish) and starts the next request directly.
CoreStartWhileToolRunning ==
  /\ corePhase = "idle"
  /\ toolState = "running"
  /\ Fault \in {"None", "BlindPublish"}
  /\ corePhase' = "streaming"
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, observedDisk,
                 editWritten, acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

\* Core reads context.jsonl at a request boundary and accepts an edit if disk
\* differs from the last published state.
CoreAccept ==
  /\ corePhase = "idle"
  /\ (toolState # "running" \/ Fault \in {"NoQuiesce", "AcceptOnly"})
  /\ corePhase' = "accepted"
  /\ observedDisk' = diskFile
  /\ IF diskFile # stateCurrent
     THEN /\ acceptedCount' = IF acceptedCount < 2 THEN acceptedCount + 1 ELSE 2
          /\ canonicalHasEdit' = TRUE
          /\ canonicalHasResult' = resultSaved
     ELSE UNCHANGED <<acceptedCount, canonicalHasEdit, canonicalHasResult>>
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, editWritten, resultSaved>>

\* Before overwriting context.jsonl, Core re-checks disk against observedDisk.
\* BlindPublish skips the re-check; AcceptOnly skips publish while a tool runs.
CoreCheckBeforePublish ==
  /\ corePhase = "accepted"
  /\ IF Fault = "AcceptOnly" /\ toolState = "running"
     THEN \* Broken half-fix skips publish after accepting, leaving state stale.
          /\ corePhase' = "streaming"
          /\ observedDisk' = "none"
     ELSE IF Fault # "BlindPublish" /\ diskFile # observedDisk
     THEN \* Concurrent edit landed after accept read disk: retry sync, never clobber.
          /\ corePhase' = "idle"
          /\ observedDisk' = "none"
     ELSE /\ corePhase' = "publishing"
          /\ UNCHANGED observedDisk
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, editWritten,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

\* Core writes the rendered context mirror and state.json, then streams.
CoreWriteMirror ==
  /\ corePhase = "publishing"
  /\ LET nextMirror == IF canonicalHasEdit THEN "merged" ELSE "v1"
     IN /\ diskFile' = nextMirror
        /\ stateCurrent' = nextMirror
        /\ corePhase' = "streaming"
        /\ observedDisk' = "none"
  /\ UNCHANGED <<toolState, editWritten, acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

CoreDispatchAsyncTool ==
  /\ corePhase = "streaming"
  /\ toolState = "none"
  /\ toolState' = "running"
  /\ UNCHANGED <<corePhase, diskFile, stateCurrent, observedDisk,
                 editWritten, acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

CoreFinishReply ==
  /\ corePhase = "streaming"
  /\ corePhase' = "idle"
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, observedDisk,
                 editWritten, acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

\* The running async tool verifies disk matches state.current, then atomically replaces context.jsonl.
\* Because tools do not hold Core's internal lock, a tool edit can land even between
\* CoreCheckBeforePublish and CoreWriteMirror if Core did not quiesce.
ToolEdit ==
  /\ toolState = "running"
  /\ ~editWritten
  /\ diskFile = stateCurrent
  /\ diskFile' = "edit"
  /\ editWritten' = TRUE
  /\ UNCHANGED <<corePhase, toolState, stateCurrent, observedDisk,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

\* The async tool completes and Core records its toolResult in canonical history.
ToolFinish ==
  /\ toolState = "running"
  /\ toolState' = "done"
  /\ resultSaved' = TRUE
  /\ canonicalHasResult' = TRUE
  /\ UNCHANGED <<corePhase, diskFile, stateCurrent, observedDisk,
                 editWritten, acceptedCount, canonicalHasEdit>>

\* An external editor writes context.jsonl outside any tool call (including between
\* CoreAccept and CoreCheckBeforePublish).
ExternalEdit ==
  /\ toolState # "running"
  /\ corePhase # "publishing"
  /\ ~editWritten
  /\ diskFile = stateCurrent
  /\ diskFile' = "edit"
  /\ editWritten' = TRUE
  /\ UNCHANGED <<corePhase, toolState, stateCurrent, observedDisk,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

Next ==
  \/ CoreStartWhileToolRunning
  \/ CoreAccept
  \/ CoreCheckBeforePublish
  \/ CoreWriteMirror
  \/ CoreDispatchAsyncTool
  \/ CoreFinishReply
  \/ ToolEdit
  \/ ToolFinish
  \/ ExternalEdit

NoLostEdit ==
  editWritten => (diskFile = "edit" \/ canonicalHasEdit)

NoDoubleAccept ==
  acceptedCount <= 1

PreservesToolResult ==
  resultSaved => canonicalHasResult

Spec == Init /\ [][Next]_vars
======================================================================
