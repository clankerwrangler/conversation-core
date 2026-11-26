----------------------- MODULE ContextFileSync -----------------------
\* Sources:
\*   src/context_file.rs
\*   src/process.rs
\*   src/store.rs
\*
\* Models synchronization between Core (accept + publish of context.jsonl)
\* and a concurrent async tool call or external editor. Core syncs at every
\* request and after each reply; both are the same Core sync steps here.
\* Not modeled: an external edit while Core publishes (after its last check,
\* before its rename). Core overwrites such an edit; ExternalEdit excludes it.
\*
\* Fault values:
\*   "None"         - Fixed protocol: accept at every sync; publish only while
\*                    no tool call is running, otherwise record the accepted
\*                    file in state.json and leave the file; refuse to
\*                    overwrite if disk changed mid-sync.
\*   "NoQuiesce"    - Earlier Core bug: publishes while an async tool call is
\*                    still executing, allowing publish to clobber its edit.
\*   "AcceptOnly"   - Broken half-fix: accepts while a tool is running without
\*                    recording it, leaving state.json stale so the same edit
\*                    is accepted twice.
\*   "Freeze"       - Earlier fix: skips accept and publish while any tool call
\*                    is running, so an edit waits until every tool finishes.
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
  canonicalHasResult,
  editDeferred

vars == <<
  corePhase, toolState, diskFile, stateCurrent, observedDisk, editWritten,
  acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred
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
  /\ editDeferred = FALSE

\* Freeze only: while a tool call runs, the sync is skipped and an edit on
\* disk is left for later.
CoreSkipWhileToolRunning ==
  /\ Fault = "Freeze"
  /\ corePhase = "idle"
  /\ toolState = "running"
  /\ corePhase' = "streaming"
  /\ editDeferred' = (editDeferred \/ diskFile # stateCurrent)
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, observedDisk, editWritten,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult>>

\* Core reads context.jsonl and accepts an edit if disk differs from the last
\* recorded publication.
CoreAccept ==
  /\ corePhase = "idle"
  /\ (toolState # "running" \/ Fault # "Freeze")
  /\ corePhase' = "accepted"
  /\ observedDisk' = diskFile
  /\ IF diskFile # stateCurrent
     THEN /\ acceptedCount' = IF acceptedCount < 2 THEN acceptedCount + 1 ELSE 2
          /\ canonicalHasEdit' = TRUE
          /\ canonicalHasResult' = resultSaved
     ELSE UNCHANGED <<acceptedCount, canonicalHasEdit, canonicalHasResult>>
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, editWritten, resultSaved, editDeferred>>

\* While a tool runs Core does not publish: it records the file it accepted
\* (AcceptOnly forgets to) and leaves the file. Otherwise it re-checks disk
\* against observedDisk before overwriting (BlindPublish skips the re-check).
CoreCheckBeforePublish ==
  /\ corePhase = "accepted"
  /\ IF toolState = "running" /\ Fault # "NoQuiesce"
     THEN /\ corePhase' = "streaming"
          /\ observedDisk' = "none"
          /\ stateCurrent' = IF Fault = "AcceptOnly" THEN stateCurrent ELSE observedDisk
     ELSE IF Fault # "BlindPublish" /\ diskFile # observedDisk
     THEN \* Concurrent edit landed after accept read disk: retry sync, never clobber.
          /\ corePhase' = "idle"
          /\ observedDisk' = "none"
          /\ UNCHANGED stateCurrent
     ELSE /\ corePhase' = "publishing"
          /\ UNCHANGED <<observedDisk, stateCurrent>>
  /\ UNCHANGED <<toolState, diskFile, editWritten, acceptedCount,
                 canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred>>

\* Core writes the rendered context mirror and state.json, then streams.
CoreWriteMirror ==
  /\ corePhase = "publishing"
  /\ LET nextMirror == IF canonicalHasEdit THEN "merged" ELSE "v1"
     IN /\ diskFile' = nextMirror
        /\ stateCurrent' = nextMirror
        /\ corePhase' = "streaming"
        /\ observedDisk' = "none"
  /\ UNCHANGED <<toolState, editWritten, acceptedCount, canonicalHasEdit,
                 resultSaved, canonicalHasResult, editDeferred>>

CoreDispatchAsyncTool ==
  /\ corePhase = "streaming"
  /\ toolState = "none"
  /\ toolState' = "running"
  /\ UNCHANGED <<corePhase, diskFile, stateCurrent, observedDisk, editWritten,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred>>

CoreFinishReply ==
  /\ corePhase = "streaming"
  /\ corePhase' = "idle"
  /\ UNCHANGED <<toolState, diskFile, stateCurrent, observedDisk, editWritten,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred>>

\* The running async tool verifies disk matches state.current, then atomically replaces context.jsonl.
\* Because tools do not hold Core's internal lock, a tool edit can land even between
\* CoreCheckBeforePublish and CoreWriteMirror if Core publishes while the tool runs.
ToolEdit ==
  /\ toolState = "running"
  /\ ~editWritten
  /\ diskFile = stateCurrent
  /\ diskFile' = "edit"
  /\ editWritten' = TRUE
  /\ UNCHANGED <<corePhase, toolState, stateCurrent, observedDisk,
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred>>

\* The async tool completes and Core records its toolResult in canonical history.
ToolFinish ==
  /\ toolState = "running"
  /\ toolState' = "done"
  /\ resultSaved' = TRUE
  /\ canonicalHasResult' = TRUE
  /\ UNCHANGED <<corePhase, diskFile, stateCurrent, observedDisk,
                 editWritten, acceptedCount, canonicalHasEdit, editDeferred>>

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
                 acceptedCount, canonicalHasEdit, resultSaved, canonicalHasResult, editDeferred>>

Next ==
  \/ CoreSkipWhileToolRunning
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

\* An edit on disk is taken in at the next sync, even while a tool call runs.
EditTakenInAtNextSync ==
  ~editDeferred

Spec == Init /\ [][Next]_vars
======================================================================
