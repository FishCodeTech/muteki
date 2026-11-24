<!-- Apply the Review criteria in CONTRIBUTING.md. Keep only relevant details. -->

Describe the problem and resulting behaviour.

Explain ownership or abstraction changes, and which contract each changed test
protects. For removed coverage, identify the duplicate or retired contract.

List checks actually run and any validation gaps.

- [ ] This change alters the wire protocol, so the next release needs a
      `PROTOCOL_VERSION` bump: a note was added under "Unreleased" above the
      constant in `crates/protocol/src/lib.rs` (the number itself changes only
      when the release is cut — CONTRIBUTING.md, principle 9).
