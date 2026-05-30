# #1496 Analysis Spike — `task(create)` auto-notify

**Author:** fixup-dev-2 · **Status:** spike (no impl) · **Base:** main @ 1c0325b

## TL;DR / Recommendation

Two pains (cheerc's "agent ignores description" + our "create→busy→send blocked")
share **one root cause**: since #1238, `task(action:create, assignee)` carries a
**second, inferior dispatch mechanism** that competes with `send(kind=task)`. It
notifies with a *title-only, non-actionable* signal.

**Primary recommendation — Option 1: `task(create)` = pure record, drop auto-notify;
`send(kind=task)` is the single dispatch+notify path.** It already auto-creates the
board entry when `task_id` is empty, so one-step dispatch still works. This fixes
**both** pains with the least code and one mental model. cheerc's Option 3 (fix the
notify format) is a narrower patch that fixes only cheerc's half and leaves our
busy-gate race intact.

---

## RCA — the mechanics (code-grounded)

There are **two** dispatch paths that both emit a `[delegate_task]` string but are
wired completely differently:

| | `send(kind=task)` → `handle_delegate_task` (comms.rs) | `task(create, assignee)` (tasks/handler.rs ~L100-135) |
|---|---|---|
| Inbox message body | **full**: `[delegate_task] {task}` + Context + success_criteria + … | **title only**: `[delegate_task] {title} (task id: {id})` |
| Notify path | daemon SEND rpc → `enqueue_with_idle_hint` | plain `inbox::storage::enqueue` + `notify_agent` |
| PTY pointer | `…kind=task from=… inbox=N` → **actionable wake** (`notification_is_actionable_wake` matches `"kind=task "`) | `[AGEND-MSG] size=N` (pointer mode) **or** `[delegate_task] {title}` (body mode) — **neither contains `kind=task `** → NOT an actionable wake |
| Busy-gate | yes (comms.rs:168-213) | none |
| Description delivery | in the message body | **nowhere in the message** — lives only in the task record; agent must `task get <id>` |

`pointer_only_inject()` is a **DaemonConfig flag** (`notify.rs:14`). In body-replace
mode the agent literally sees `[from:creator] [delegate_task] <title> (task id)` —
which matches no `[AGEND-MSG]` rule the agent is trained on (instructions.rs:307+),
so it's read as conversational text → agent acts on the *title*, never opens the
inbox, never does `task get` → never sees the description (cheerc's verification-code
proof).

## Q1 — design intent of the `[delegate_task]` PTY format

Added by **#1238 "feat(tasks): auto-dispatch on create with assignee"** (3c85287,
2026-05-26). Intent: make `task create assignee:X` *also* dispatch, sparing a
separate `send`. But it predates / sidesteps the `enqueue_with_idle_hint` actionable-
pointer convention, so it shipped a half-integrated notify: title-only body + a
non-actionable pointer. It was **not** designed expecting the agent to go `task list`
— it expected the injected line itself to be the dispatch. That assumption is exactly
what breaks: the line isn't in the format the agent treats as "go read inbox," and
even if it were, the inbox copy lacks the description.

## Q2 — should `task(create)` auto-notify at all? (the core fork)

**No — notify should be `send(kind=task)`'s job; the board should be a pure record.**
Reasoning:
- The fleet protocol already says kind=task dispatch *requires* a task_id (obtained
  via create). The intended shape is **create (record) → send(kind=task) (dispatch
  with context)**. In that shape, create auto-notifying is redundant and harmful.
- `send(kind=task)` already auto-creates the board entry when `task_id` is empty
  (comms.rs:305-306), so "create + dispatch in one call" is fully served by a single
  `send(kind=task)` — no functionality is lost.
- Our whole team dispatches via `send(kind=task)`; create's auto-notify only ever
  *interferes* (see Q4).

The only thing lost is cheerc's "create-with-assignee as a lightweight assignment
ping." That use case is better served by `send(kind=task)` (rich + actionable) or, if
we truly want a record-only nudge, by a deliberate actionable pointer (Q3) — not the
current half-baked inline title.

## Q3 — is cheerc's Option 3 (auto-notify uses `[AGEND-MSG-PENDING]` pointer) correct?

**Necessary but not sufficient, and it doesn't address our pain.**
- It fixes cheerc's *format* problem: an actionable `…kind=task… (use inbox tool)`
  pointer makes the agent follow its trained inbox-reading habit.
- BUT the inbox message still carries only the **title**. Reading the inbox yields no
  description; the agent must additionally `task get <id>`. So Option 3 must be paired
  with **either** embedding the description in the inbox body **or** an explicit
  "run `task get <id>` for details" instruction — otherwise the verification-code test
  still fails.
- It does **nothing** for our busy-gate race (Q4): create still fires a premature
  wake.

So Option 3 alone is a partial fix. If we keep auto-notify, it must be Option 3 **+**
description delivery **+** a busy-gate fix.

## Q4 — our pain RCA: create→busy→`send(kind=task)` blocked→forced force-send

Sequence:
1. lead `task create assignee:dev-2 [branch:X]` → record created **+ auto-notify**
   wakes dev-2.
2. dev-2 (trained) claims/starts → task → `Claimed`/`InProgress`.
3. lead's follow-up `send(kind=task, instance:dev-2 [branch:X])` carrying the real
   context hits the busy-gate (comms.rs:168-213): target has an active task →
   `busy:true` (or `dispatch rejected … already has active task on branch X` when a
   branch is supplied, comms.rs:178-191).
4. lead must resend with `force=true` + `force_reason`.

**Root cause:** create's auto-notify is a *premature, context-poor* wake that pushes
the agent into the busy state **before** the real, context-rich dispatch arrives. The
busy-gate then correctly (from its view) blocks a "second dispatch" — but it's not a
second dispatch, it's *the* dispatch finally delivering its payload. Two mechanisms
racing on one logical dispatch.

Under Option 1 this disappears: there is no premature wake; the single
`send(kind=task)` creates+notifies atomically, so the busy-gate never sees a
half-dispatched task.

(Orthogonal hardening, valuable regardless: the busy-gate could exempt a
`send(kind=task)` whose `task_id` == the target's current active task — enriching an
in-flight dispatch is not a competing one. Worth a follow-up even if we pick Option 1.)

## Q5 — options, trade-offs, KISS, RED sketch

### Option 1 (RECOMMENDED) — drop create auto-notify; `send(kind=task)` is the sole dispatch
- **Fixes:** both pains. One dispatch path, one notify format, one busy-gate.
- **Cost:** behavior change for #1238 create-dispatch users; `task create assignee:X`
  no longer pings. Mitigated: `send(kind=task)` auto-creates the board row, so the
  one-call dispatch is preserved (just via `send`, not `create`).
- **Blast radius:** delete the notify/enqueue/track_dispatch block in
  tasks/handler.rs (~30 lines). Nothing else reads it as a contract.
- **KISS:** highest. Removes a whole parallel mechanism.
- **RED sketch:** `task(create, assignee:other)` ⇒ assert **no** inbox message
  enqueued for `other` and `notify_agent` not invoked (no dispatch side-effect);
  `send(kind=task)` path unchanged (still creates row + actionable notify). RED today
  (auto-notify fires).

### Option 2 — cheerc Option 3 only (pointer format)
- **Fixes:** cheerc's format. **Not** the description gap, **not** our race.
- **KISS:** medium. Smallest diff but leaves dual mechanism + race.
- **RED sketch:** assert task-create's injected PTY line contains `kind=task ` /
  `[AGEND-MSG-PENDING]` (actionable), not a raw `[delegate_task]` body. RED today.

### Option 3 — unify: create-with-assignee routes through the `send(kind=task)` path
- task-create delegates to `handle_delegate_task` (full body from `description`,
  actionable pointer, busy-gate). Effectively create-with-assignee *becomes*
  send(kind=task).
- **Fixes:** cheerc (rich actionable) and the race (create *is* the full dispatch, so
  no follow-up send → nothing to block) — **iff** callers stop doing create-then-send.
- **Cost:** task-create grows dispatch args (success_criteria, context); two entry
  points converge on one impl (good) but the surface is larger than Option 1.
- **KISS:** medium. Elegant unification, more moving parts than deletion.
- **RED sketch:** `task(create, assignee, description:"CODE-123")` ⇒ assert the
  target's inbox body contains `CODE-123` (description delivered) and the pointer is
  actionable. RED today (only title delivered).

### Recommendation
**Option 1**, plus the orthogonal busy-gate `task_id`-exemption as a small follow-up.
If the team wants to preserve create-as-one-step-dispatch, **Option 3** is the elegant
second choice; **Option 2** is insufficient alone. Avoid shipping Option 2 by itself —
it reads as "fixed" while the description gap and our force-send tax both remain.
