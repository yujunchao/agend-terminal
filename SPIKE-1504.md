# #1504 Analysis Spike — Windows shim recursive-spawn storm

**Author:** fixup-dev-2 · **Status:** spike (read-only, no impl) · **Base:** main @ 76cb5ba
**Impl gate:** waits for #1511 (same file `agend-git.rs`) to merge first.

## TL;DR

Operator's 3-layer RCA is **correct against current code** (line refs below). The
three fixes are sound; I add precise code for each, plus the load-bearing answer to
"how do we RED-test Windows-only behavior on a Mac/Linux dev box" — **windows-latest
IS in the CI matrix**, and the recursion guard is fully cross-platform testable.

## RCA verification (against current code)

**The recursion chain (Windows):**
1. **Layer 1 — `AGEND_REAL_GIT` never gets set.** `src/agent/mod.rs` (the
   `if std::env::var("AGEND_REAL_GIT").is_err()` block, ~L697-709) builds the git
   search path with **`.split(':')`** — hardcoded Unix separator. On Windows PATH is
   `;`-separated AND entries contain drive-colons (`C:\Program Files\Git\cmd`), so
   `.split(':')` shreds PATH into garbage (`["C", "\Program Files\Git\cmd;D", ...]`).
   `which::which_in("git", …)` then fails → the daemon never injects `AGEND_REAL_GIT`
   into the agent env. (Confirmed: the block uses `.split(':')` and a string
   `*p != agend_bin` filter.)
2. **Layer 2 — the shim resolves to itself.** The agent runs `git …`; PATH has
   `$AGEND_HOME/bin` (the shim installed as `git`) → resolves to the shim. The shim's
   `resolve_real_git()` (`src/bin/agend-git.rs`, the fn around L959+) Priority 1 reads
   `AGEND_REAL_GIT` — unset (Layer 1) → falls to Priority 2: `which_in` excluding
   `agend_bin` via **`*p != agend_bin`** where `agend_bin = format!("{h}/bin")`
   (forward slash). Windows PATH entries are backslash + case-variant + maybe
   trailing-slash, so the string compare **fails to exclude** → `which_in("git")`
   finds the shim in `$AGEND_HOME/bin` → returns the **shim's own path**.
   (Note: this fn's separator is already `cfg!(windows)`-aware — `;` — so Layer 2's
   ONLY bug is the string self-exclusion, not the split.)
3. **Layer 3 — no recursion brake.** `exec_real_git()` (L929) and
   `exec_with_conflict_guidance()` (L901) just `Command::new(resolve_real_git())…`.
   On **Windows** the non-unix arm uses `cmd.status()` (spawn + wait), not `exec()`
   replace — so each self-resolution spawns a NEW shim → **unbounded process storm**
   (a fork bomb), not a single replaced process. There is no depth sentinel; the only
   anti-recursion mechanism is `AGEND_REAL_GIT`, which Layer 1 defeated.

Healthy path on Unix masks all of this: `:` splits correctly → `AGEND_REAL_GIT` set →
Priority 1 hits → real git; and even a miss `exec()`-replaces rather than storms.

## Fix design

### ① `agent/mod.rs` — `split_paths` + Path-normalized exclusion
Replace the `.split(':')` + string filter with the platform-aware std API and a
normalized exclusion. Extract a pure helper so it's unit-testable:

```rust
/// Build the git-search PATH with `$AGEND_HOME/bin` (the shim dir) removed,
/// using the platform PATH separator. Pure for testability.
fn git_search_without_shim(path: &OsStr, shim_dir: Option<&Path>) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !same_dir(p, shim_dir))
        .collect()
}
// caller:
let shim_dir = home.map(|h| h.join("bin"));
let path_os = std::env::var_os("PATH").unwrap_or_default();
let search = std::env::join_paths(git_search_without_shim(&path_os, shim_dir.as_deref()))
    .unwrap_or_default();
if let Ok(git_path) = which::which_in("git", Some(search), ".") {
    cmd.env("AGEND_REAL_GIT", git_path);
}
```
Key points: `split_paths` (handles `;` on Windows AND drive-colons AND quoted
entries); `var_os` (PATH need not be UTF-8); exclusion via `same_dir` (below).

### ② `agend-git.rs::resolve_real_git` — same normalized exclusion
Layer 2's separator is already correct, so the surgical fix is ONLY the exclusion —
but reuse the same helper for consistency:

```rust
fn same_dir(a: &Path, b: Option<&Path>) -> bool {
    let Some(b) = b else { return false };
    // Prefer canonical (resolves slash dir, case-fold on Windows NTFS, symlinks);
    // fall back to a lexical compare when a path doesn't exist on disk.
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => normalize_lexical(a).eq_ignore_ascii_case_on_windows(&normalize_lexical(b)),
    }
}
```
`canonicalize` is the load-bearing fix: on Windows it returns the case-normalized,
backslash, no-trailing-slash form for both sides, so `C:\Users\..\bin` (PATH) and
`C:/Users/../bin` (`format!("{h}/bin")`) compare equal. The lexical fallback covers
the rare case where `$AGEND_HOME/bin` doesn't exist yet.

### ③ Recursion guard — depth sentinel (cross-platform brake)
Defense-in-depth: even if ①② regress again, cap the recursion.

- **Env:** `AGEND_GIT_SHIM_DEPTH` (integer).
- **Check (very top of `main()`, BEFORE `should_bypass` — bypass also execs real git):**
  ```rust
  const MAX_SHIM_DEPTH: u32 = 3;
  let depth = env::var("AGEND_GIT_SHIM_DEPTH").ok()
      .and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
  if depth >= MAX_SHIM_DEPTH {
      eprintln!("agend-git: FATAL recursion guard (depth={depth}) — the shim resolved \
                 to itself; AGEND_REAL_GIT is unset/unresolvable. Set AGEND_REAL_GIT to \
                 the real git binary, or check $AGEND_HOME/bin is excluded from PATH. (#1504)");
      std::process::exit(70); // EX_SOFTWARE
  }
  ```
- **Increment:** every real-git spawn (`exec_real_git`, `exec_with_conflict_guidance`)
  sets `cmd.env("AGEND_GIT_SHIM_DEPTH", (depth + 1).to_string())` on the child. The env
  propagates across both `exec()` (unix) and `status()` (windows), so the brake works
  on both. Healthy operation never exceeds depth 1 (real git ≠ shim → no re-entry), so
  `MAX=3` has zero false-trip risk.

## ⑤ RED test strategy — the key question

| Layer | Cross-platform RED on Mac/Linux CI? | How |
|---|---|---|
| ① split/exclusion | **Partly** | Extract `git_search_without_shim` (pure). Unit-test the EXCLUSION cross-platform (PATH with the shim dir in slash/case/trailing-slash variants → assert removed). The SEPARATOR correctness is delegated to std `split_paths` (already tested upstream); enforce its use with a **grep-invariant** test (#1476/#1502 pattern) forbidding `.split(':')` on PATH in `agent/mod.rs`. |
| ① separator end-to-end | **Yes, on windows-latest** | The CI matrix is `[ubuntu, macos, windows-latest]`, so a `#[cfg(windows)] #[test]` exercising a real `C:\…;C:\…` PATH runs on the windows job. Belt-and-suspenders to the grep-invariant. |
| ② self-exclusion | **Yes** | Same pure-helper unit test for `same_dir`: assert `C:/a/bin` vs `C:\a\bin` compare equal under canonicalize/lexical. The case-fold branch is `#[cfg(windows)]`. |
| ③ recursion guard | **Yes, fully portable — strongest RED** | Integration test: build the `agend-git` bin (or call an extracted `guard_should_abort(depth) -> bool`), invoke with `AGEND_GIT_SHIM_DEPTH=3` + a harmless arg (`--version`), assert it exits `70` with the guard stderr. No Windows needed — pure env logic. RED before the guard exists (infinite/΄no-exit); GREEN after. |

**Bottom line:** the recursion guard (③) gives the cleanest, fully-portable RED that
proves the P0 storm is contained; ①② lean on extracted pure helpers (exclusion is
cross-platform; separator is std-guaranteed + grep-invariant + windows-CI smoke).

## ⑥ Merge order / conflict surface vs #1511

- **#1511** edits `agend-git.rs::classify()` (+ maybe a one-line mutating-arm token).
- **#1504** edits `agent/mod.rs` (Layer 1 — no overlap), and in `agend-git.rs`:
  `resolve_real_git()` + top-of-`main()` (guard) + `exec_real_git` /
  `exec_with_conflict_guidance` (depth-increment). **Different functions** from
  `classify()`.
- Textual conflict risk is **low** (disjoint fns), but it's the same file, so per
  lead's sequencing: **let #1511 merge first, then #1504 rebases onto it and impls.**
  The spike (read-only) needs no gate. `agent/mod.rs` is #1504-exclusive.
- One watch-item: if #1511 also inserts near the top of `main()` (it shouldn't — its
  change is in `classify`), the guard insertion could touch adjacent lines; a clean
  rebase resolves it. Recommend #1504 rebase + re-run the diff-stat-vs-fresh-origin
  check (the #1508 lesson) before pushing.

## KISS assessment

- ①② are net **simplifications**: `split_paths`/`canonicalize` replace bespoke
  string-splitting/compare with std primitives that are correct by construction. Lower
  line count, fewer edge cases.
- ③ is ~10 lines of pure additive defense — justified for a 🔴 P0 fork-bomb; the env
  sentinel is the simplest possible brake (no shared state, no IPC).
- Extracting two pure helpers (`git_search_without_shim`, `same_dir`) is the only
  "new surface" and it exists solely to make the bug unit-testable — worth it.
- Recommend shipping all three together (they're one causal chain; ①② fix the cause,
  ③ caps the blast radius if it ever regresses).
