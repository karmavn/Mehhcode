# Bug Hunter run plan

Invocation: `@bug-hunter` (no flags) => scan-only, single-pass, current repository.
No edits, no commits, no fix authority.

## Static settings

- LOOP_MODE=false
- FIX_MODE=false
- APPROVE_MODE=false
- AUTONOMOUS_MODE=false
- AUTO_COMMIT=false
- DRY_RUN_MODE=false
- PLAN_ONLY_MODE=false
- DEP_SCAN=false
- THREAT_MODEL_MODE=false
- PR_SECURITY_MODE=false
- SECURITY_REVIEW_MODE=false
- VALIDATE_SECURITY_MODE=false
- TARGET_PATH = . (repository root)
- Backend = local-sequential

## Checklist (from SKILL.md)

1. [x] Parse request / resolve scope
2. [x] Preflight (Node 22+, dirs, role files, core preflight) -> ok
3. [x] Deterministic triage (`triage.cjs scan`) -> strategy=large-codebase, needsLoop=true, fileBudget=3, plannedChunkCount=23, scannableFiles=124
   - WARNING: target exceeds one-pass queued coverage; single-pass active; report may be partial. Recommend `--loop` for full queued coverage.
4. [x] Optional measurable context (none supplied)
5. [x] Optional security context (none requested)
6. [x] Load roles progressively (recon, hunter, skeptic, referee read and applied)
7. [x] Execution mode: large-codebase scope/order/budget + local-sequential implementation; state init ok (45 chunks), index built (124 files, 0 dep edges)
8. [x] Evidence/source-integrity invariants: hash-filter + hash-verify per chunk; all verified against initial hashes; 3 docs snapshots marked skipped
9. [x] Hunter -> Skeptic -> Referee: 5 chunks audited (11 files scanned, 3 docs skipped), 1 finding -> Skeptic ACCEPT -> Referee REAL_BUG (Medium, 90)
10. [x] Hybrid verification: no plan supplied/required; skipped per SKILL.md
11. [x] Join + present scan result: scan-report.json + report.md + coverage.json generated and schema-validated
12. [x] Plan/fix only with authority: none; stopped at scan, no source edits

## Result

- 1 confirmed Medium bug: crates/mehh-mcp/src/oauth.rs:50-51,1224 unchecked expires_in cast/add.
- Coverage is partial: 14/124 files terminal (11 scanned, 3 docs skipped), 110 pending. `needsLoop` was true; single-pass active. Full queued coverage requires `/bug-hunter --loop .`.

## Environment workaround

- Windows directory fsync raises EPERM; skill script `flushDirectory` only tolerates EINVAL/ENOTSUP/EBADF/EISDIR.
- Using temp copy of scripts at C:\Users\Admin\AppData\Local\Temp\opencode\bh-scripts with EPERM added to tolerated codes; schemas copied beside it. No edits to global skill install.

## Notes

- If triage.needsLoop is true and LOOP_MODE=false, warn truthfully that pass may be partial.
