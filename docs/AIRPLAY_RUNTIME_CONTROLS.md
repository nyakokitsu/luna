# Add permission-scoped native controls and bounded station-volume I/O

## Summary

- Add token/permission-scoped native-player pause/release endpoints, separate
  from the media-provider broker, so native handover cannot target AirPlay itself.
- Run the stock CLI in a bounded worker with ownership, queue/deadline checks,
  finite session lease and process-group cleanup. Queue/startup share one
  absolute request deadline; expired requests do not submit a late pause.
- Add a narrow KEY_ALICE long-poll endpoint with independent event-reader
  fan-out, fresh invocation readers and permission checks before AND after wait.
- Reuse parsed manifests only after content hashing; retain token revocation
  and per-request permission validation.
- Isolate volume backend I/O from async workers, bound helper output/time,
  serialize actual operations even after caller cancellation, and coalesce
  only concurrent in-flight reads. Do not cache completed volume answers.

## Public v0.2.23 preserved

Ported onto the exact public v0.2.23 commit
b6ddd7f4e70f985256d01343899697a076f6ce05, not an older fork with matching
version text. Public src/audio.rs, src/audio_jobs.rs, src/owner.rs and src/lib.rs
are byte-identical to the tag. Audio/owner routes, cancellable playback and
graceful audio-job shutdown remain wired in gateway.rs.

## Contract

- POST /api/runtime/:id/native-player/pause (media.native).
- POST /api/runtime/:id/native-player/release (media.native).
- POST /api/runtime/:id/input/alice/next (input.alice).
- Capabilities advertised through existing runtime media endpoint.
- Native pause reports submission, NOT independently verified audible pause.
- No raw speech, vendor CLI output or runtime credentials added to diagnostics.
- No broad input permission, EVIOCGRAB, provider redirection or weakened package
  validation, permission validation, filesystem or readback checks.

## Explicitly out of scope

No LED/framebuffer/colour/brightness changes. No public version bump, build
assets, release, registry update, firmware image or live deployment.

## Validation

- Full cargo test --locked --offline --workspace: PASS (135 passed, 1 ignored; peak 61 C; cleanup confirmed).
- Public audio/owner source and gateway integration preservation: checked.
- Exact rebased emulator lifecycle, live button/voice/playback, performance
  improvement and hardware support beyond unit tests: NOT_RUN / not claimed.
- Historical tests of older local branches do not qualify this port. Earlier
  FAIL/NOT_RUN outcomes and two-mode CPU observations are preserved separately.

## Merge / release order

Land and release the Luna API first; then publish the companion AirPlay
adapter with generated package metadata requiring that new Luna release.
Public v0.2.23 alone is not an adequate API dependency.

## Submission status

Submitted as https://github.com/nyakokitsu/luna/pull/29 from the ryysel fork.
The companion AirPlay PR is not submitted yet.
