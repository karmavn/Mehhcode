# Bug Hunter Report

- Findings reviewed: 1
- Confirmed: 1
- Dismissed: 0
- Manual review: 0
- Unreviewed: 0

## Confirmed Bugs
- BUG-1 | Medium | crates\mehh-mcp\src\oauth.rs | Untrusted OAuth `expires_in` (u64) is cast to i64 and added to the current time without saturation, so extreme provider values wrap the stored token expiry into the past and force repeated refreshes (or an overflow panic in debug builds).
  Confidence: 90 (high) | INDEPENDENTLY_VERIFIED
  Analysis: Quote check passed: crates/mehh-mcp/src/oauth.rs:50-51 is exactly `pub fn mcp_token_expiry_seconds(expires_in: u64) -> i64 { (mehh_ai::now_ms() / 1000) + expires_in as i64 }` and :1224 is `let expire_at = new_tokens.expires_in.map(|exp| now + exp as i64);`. Independently traced both paths: OAuthTokens.expires_in (:40) is `Option<u64>` filled directly from token-endpoint JSON at :1125-1127 (refresh) and at crates/mehh-coding-agent/src/mcp_manager.rs:520-523 (initial authorization). Neither path bounds the value before the cast; repo-wide search found no clamp. `exp as i64` wraps for exp > i64::MAX (e.g. u64::MAX -> -1), producing tokens_expire_at in the past (:1229 / mcp_manager.rs:536). StoredMcpBearerProvider::bearer_token at oauth.rs:1251-1272 compares that value against now+30 and therefore re-enters do_refresh on every call, yielding repeated token requests instead of reusing a live token; in debug builds the addition can panic on overflow. The sibling module crates/mehh-ai/src/oauth.rs:169-173 uses saturating_mul/saturating_add explicitly for hostile provider values, confirming the intended hardening is absent here. No counter-evidence blocks the chain. Rubric applied: Medium - silent failure in a reachable edge case with a wrong result; not High because it needs a nonstandard extreme provider value rather than a common valid input, and not Critical because there is no unauthenticated exploit or default-path data loss.

## Manual Review
- None

## Dismissed Findings
- None

## Unreviewed Findings
- None

## Coverage and limitations

- Mode: scan-only, single-pass (no edits, no fix authority).
- Files with terminal coverage: 14 / 124 ({"done":14,"pending":110}).
- Triage reported needsLoop=true; single-pass was active. This report may be partial.
- Run `/bug-hunter --loop .` for complete queued coverage of the remaining 110 files.
- Documentation snapshots under docs/handoff/**/raw/** were marked skipped as context-only.
- No dependency scan or threat model was requested; no hybrid verification plan was supplied.
