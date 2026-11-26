# Changelog

## Unreleased

### Documentation
- Document owner Mac verification without hosted CI
- Clarify public macOS build instructions
- Merge bootstrap review corrections
- Update terminal chat changelog
- Record provider safety and explicit telemetry consent
- Record verified CLI release preparation


### Features
- Add native Asodya sign-in and OS keyring storage
- Merge native Asodya sign-in
- Add Cortex chat streaming and inline activity view
- Use accepted output identity for cancellation and reconnect
- Merge inline terminal chat
- Add local hold-to-talk input (#10)
- Add external provider API key support
- Integrate reviewed external provider adapters
- Add explicit consent-gated CLI usage tracking
- Integrate explicit consent and fixed usage payloads
- Prepare checksum-verified release tooling
- Prepare verified CLI installation artifacts
- Integrate reviewed local speech input (#10)


### Fixes
- Discard invalid successful handoffs before session storage
- Keep narrow terminal activity details readable
- Correlate streamed results with the selected conversation
- Keep terminal title readable on light backgrounds
- Stop voice capture and cancel transcription
- Discard transcript after accepted cancellation
- Bound and sanitize provider responses
- Reject unsafe release archives
- Clean archive after manifest publish failure
- Stop fallback voice recording after pause


### Maintenance
- Initialize feature integration branch
- Bootstrap Cortex CLI layers and native build checks
- Merge Cortex CLI bootstrap
- Refresh sign-in formatting and changelog


### Tests
- Reject mismatched native handoff credentials in fixtures
- Cover chat history scope and fractional timestamps
- Verify terminal detail toggle states through real key input
- Cover local cancellation and transcript privacy


### merge
- Refresh voice input from accepted feature base


### refactor
- Keep chat resume rules in the domain service
- Share gateway settings with the chat transport

