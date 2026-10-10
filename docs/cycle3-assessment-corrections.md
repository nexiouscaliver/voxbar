# Cycle 3 assessment corrections (AUD-15)

The cycle 3 assessment lives outside this worktree
(`voxbar-build-docs/cycle3-assessment.md`, not checked out on
`cycle3/audit-fixes`), so its two label errors are corrected here instead of
in place. The severity source of truth is the goldmine
(`docs/quality-goldmine.md`); the wave-1 entries it marks fixed were flipped
to the FIXED convention there in the same change.

## 1. Wave 1 is not "All P1" — KB-158/187/190/193 are P2

The assessment's wave-1 section says: "One-scene fixes with clear evidence,
no design decision needed. All P1." That blanket claim is wrong for four of
the twelve wave-1 items: KB-158, KB-187, KB-190 and KB-193 sit in the
goldmine's `## P2` section (round 3 filed them as P2/low), not in `## P1`.

Corrected labeling: wave 1 = 8 P1 items (KB-027, KB-031, KB-033, KB-034,
KB-107, KB-151, KB-154, KB-185) + 4 P2 items (KB-158/187/190/193). Both the
"all P1" sentence and any per-item P1 label on those four ids should read
P2. Severity labels must come from the goldmine section the id lives in, not
from the wave that happens to fix it.

## 2. Wave 2's enumeration must carry KB-162 and KB-163

The assessment's wave-2 class list enumerates: "KB-011 residual, 013
residuals, 016, 020, 029, 038, 061, 087, 106, 124, 139, 143, 148, 155,
163, 180, 182, 184". Two corrections:

- KB-163 is listed, but only as a bare member; its goldmine anchor is the
  theme-1 tail entry "+ device-name KB-163", and the wave-2 design bullets
  never name it — it must be carried explicitly as wave-2 scope.
- KB-162 is missing from the enumeration altogether. It appears only inside
  the DESIGN bullets ("KB-162: overlay stops wiping the notice on preview
  show"), so the class list under-counts the wave.

Corrected wave-2 enumeration: KB-011 residual, 013 residuals, 016, 020,
029, 038, 061, 087, 106, 124, 139, 143, 148, 155, **162, 163**, 180, 182,
184. Both were wave-2 scope in practice: KB-162 landed in a6997aec (the
final preview no longer wipes the same session's tail notice); KB-163
(companion_disconnected drops its device-name detail) remains open.
