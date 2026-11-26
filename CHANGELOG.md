# Changelog

## Unreleased

### Documentation
- Document owner Mac verification without hosted CI
- Clarify public macOS build instructions
- Merge bootstrap review corrections
- Update terminal chat changelog
- Record provider safety and explicit telemetry consent
- Record verified CLI release preparation
- Record partial write outcome fix
- Record reviewed local voice input (#10)
- Record reviewed subagent workflow
- Update selected-note omissions changelog
- Record reviewed selected note context (#7)


### Features
- Add native Asodya sign-in and OS keyring storage
- Merge native Asodya sign-in
- Add Cortex chat streaming and inline activity view
- Use accepted output identity for cancellation and reconnect
- Merge inline terminal chat
- Add phone control for interactive sessions
- Consume typed tool requests
- Execute output-bound approved tool actions
- Add external provider API key support
- Integrate reviewed external provider adapters
- Add explicit consent-gated CLI usage tracking
- Integrate explicit consent and fixed usage payloads
- Prepare checksum-verified release tooling
- Prepare verified CLI installation artifacts
- Consume bounded child runs in CLI
- Add local hold-to-talk input (#10)
- Integrate reviewed local speech input (#10)
- Integrate reviewed bound child execution and approvals (#5)
- Add explicit Annotations note selection
- Attach selected notes as temporary context


### Fixes
- Normalize owner metadata in native CLI archives
- Discard invalid successful handoffs before session storage
- Keep narrow terminal activity details readable
- Correlate streamed results with the selected conversation
- Keep terminal title readable on light backgrounds
- Bound read and search behavior
- Retry remote approval decisions safely
- Retry owned phone approval acknowledgement
- Canonicalize approval action digest
- Bound and sanitize provider responses
- Reject unsafe release archives
- Clean archive after manifest publish failure
- Report partial file action writes accurately
- Stop voice capture and cancel transcription
- Discard transcript after accepted cancellation
- Stop fallback voice recording after pause
- Preserve bound child lifecycle through parent EOF
- Keep child tracked through parent stream errors
- Retain child until terminal confirmation
- Retain child confirmation through common cleanup (#5)
- Bind turn events to confirmed child turns
- Disclose omitted selected-note blocks
- Emit terminal when turn stream never starts


### Maintenance
- Initialize feature integration branch
- Bootstrap Cortex CLI layers and native build checks
- Merge Cortex CLI bootstrap
- Refresh sign-in formatting and changelog
- Update selected notes changelog


### Tests
- Reject mismatched native handoff credentials in fixtures
- Cover chat history scope and fractional timestamps
- Verify terminal detail toggle states through real key input
- Cover replayed prompt deduplication
- Verify cursor reconnect behavior
- Verify first remote approval decision wins
- Cover local cancellation and transcript privacy
- Cover parent error with live child tracker


### merge
- Refresh accepted base for approval tools
- Refresh voice input from accepted feature base
- Refresh selected notes from accepted feature base


### refactor
- Keep chat resume rules in the domain service
- Share gateway settings with the chat transport
