# `wip/bughunt` reconciliation — 2026-08-11

The branch diverged from `master` at `fec28a4` and contained 11 commits. `git
cherry` reported all 11 as patch-distinct because both branches evolved
independently, so each change was compared semantically against current code
and tests. No commit should be merged or cherry-picked as a whole.

| Branch commit | Finding on current `master` | Decision |
| --- | --- | --- |
| `c2106c7` — isolate dictionary rules | Claims-based replacement and reciprocal-rule tests exist (`d05905f`) | Already covered |
| `3175c1f` — retain a newer download retry | Cleanup checks the watch channel identity (`3b6178a`) | Already covered |
| `89b1de0` — private files while writing | Config, history, token, and hotword writes were safe separately, but duplicated | Preserved as the shared `persistence::write_private` helper and adversarial tests |
| `b249208` — one completion announcement | Only the request that starts the monitor owns completion (`69b6955`) | Already covered |
| `3b89a19` — correct two-channel rate | Realtime and local Parakeet downmix complete frames (`415e153`, `4fe38d9`, `3f51e36`) | Already covered |
| `43e43bd` — model-directory-only deletion | Traversal and symlink defenses existed, but an arbitrary leaf below the model root was still accepted | Preserved by restricting deletion to the immutable model catalogue, with unit and D-Bus tests |
| `5c732a2` — defaults for partial config | Provider sections default independently (`0074a3d` and later coverage) | Already covered |
| `9c83693` — reject inert shortcuts | Central validation rejects unmodified and modifier-only shortcuts (`3eff1e6`, `3b18197`) | Already covered, more broadly |
| `a3d6e9f` — preserve partial realtime text | Provider and transport failures retain partial text (`3e27c4e`, `a9c8cd3`) | Already covered |
| `d3877ef` — salvage damaged history | Entry salvage, whole-file preservation, and unique backups exist (`61fde92`, `5328308`, `136ed30`) | Already covered, more broadly |
| `3f02476` — serialize simultaneous downloads | Startup is atomic under the shared-state lock (`2bb5db2`) | Already covered |

The useful deltas were reimplemented against current code because the old
patches would discard newer safeguards. After the complete automated gate
passed, the clean linked worktree and local `wip/bughunt` branch were deleted.
Its final tip was `3f02476192b834fc0eaea1b9b333a7b4f04f00cb`; this audit retains
the decisions even after the branch ref disappears.
