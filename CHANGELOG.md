# Changelog

## Unreleased

### Documentation
- Document owner Mac verification without hosted CI
- Clarify public macOS build instructions
- Merge bootstrap review corrections
- Update terminal chat changelog


### Features
- Add local hold-to-talk input (#10)
- Prepare versioned CLI archives and checksum-verified Linux/macOS installation
- Add native Asodya sign-in and OS keyring storage
- Merge native Asodya sign-in
- Add Cortex chat streaming and inline activity view
- Use accepted output identity for cancellation and reconnect
- Merge inline terminal chat
- Add external provider API key support
- Integrate reviewed external provider adapters
- Add explicit consent-gated CLI usage tracking
- Integrate explicit consent and fixed usage payloads


### Fixes
- Reject non-executable release entries and clean failed installer/package staging
- Discard invalid successful handoffs before session storage
- Keep narrow terminal activity details readable
- Correlate streamed results with the selected conversation
- Keep terminal title readable on light backgrounds
- Bound and sanitize provider responses


### Maintenance
- Initialize feature integration branch
- Bootstrap Cortex CLI layers and native build checks
- Merge Cortex CLI bootstrap
- Refresh sign-in formatting and changelog


### Tests
- Reject mismatched native handoff credentials in fixtures
- Cover chat history scope and fractional timestamps
- Verify terminal detail toggle states through real key input


### refactor
- Keep chat resume rules in the domain service
- Share gateway settings with the chat transport

