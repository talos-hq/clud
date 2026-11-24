# Codex bridge compatibility after #2932

## What happened

The Claude harness selected GPT-6.2 Sol on the ChatGPT subscription route.
The bridge advertised Codex CLI 0.164.0, last verified with GPT-6.1. The
subscription backend rejected the request with HTTP 400 and
`model_not_found`. The previous session and a fresh reproduction recorded the
same rejection. GPT-6.1 Terra worked in the same harness. Native Codex 0.171.0
and the rebuilt bridge advertising 0.171.0 both completed a GPT-6.2 Sol turn.
PR #2940 restored the route. Issues #1490 and #2299 had already required
compatibility-version updates.

Public catalogs and the installed Codex App Server establish model naming and
availability independently of the bridge's wire identity. Those checks could
advance the model while an installed bridge kept its old identity. The
previous tests checked agreement among the README, Docker pin, and request
headers. That agreement held even when the upstream rejected the pair.
The classifier retained a scrubbed reason for forensic logs and stopped at
the 400, which the harness displayed as a generic upstream error.

## Refactor and recovery plan

1. Put the verified subscription client version in the existing validated
   server settings document. Use a bundled value when the document is absent,
   invalid, or disabled. Keep the separately installed Bosn CLI pin tied to the
   recorded live check. A native CLI version or a public model listing is
   insufficient evidence to publish a bridge identity.
2. Use one constructor for the shared wire identity across Responses,
   compaction, and other existing Codex requests. Preserve the header
   differences and the bundled identity on other providers.
3. For an exact upstream `model_not_found` HTTP 400 or 404 on an official
   subscription Responses request, refresh the compatibility section under a
   bounded process-wide cooldown. If the verified version differs from the
   version sent, retry the same request once, before any downstream output.
   Keep auth, endpoint, model, effort, and request identity bound to the same
   turn. Leave ordinary retry and credential recovery rules in place. Surface an
   actionable subscription-model error when there is no compatible update.
   Compaction, images, generic 400s, metered routes, and committed responses
   do not enter this recovery path.
4. Test old-version rejection and recovery with scripted upstream responses,
   plus unchanged metadata, exact error matching, non-subscription requests,
   cancellation, and retry limits. Verify a native and a bridge request against
   the subscription backend before publishing an identity change. Record the
   model, version, date, and outcome without storing credentials or prompts.

A verified settings update can recover released clients that include this
mechanism, including processes that started with stale settings. An upstream
compatibility change for which no verified identity has been published still
needs an operator to qualify the replacement. Model fallback would change a
user selection and needs a separate policy for automatic defaults and saved
or explicit choices.

## Qualifying an update

Record a successful authenticated turn for the affected model in the
candidate native CLI. Then exercise the real bridge entry point with the
candidate settings document and the same subscription and model. For the
original incident, use `clud --codex --harness claude --model gpt-6.2-sol -- -p 'Reply only with OK.'`.
Compare the bridge result and sanitized upstream diagnostic with the current
settings as a control. Update `codex_compatibility.client_version` only after
the bridge succeeds. Keep the bundled record and Bosn pin in sync when
updating the bundled release. Existing clients that support the section can
consume the verified setting without reinstalling clud.
