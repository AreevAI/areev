# Anonymization fixtures

An egress control you cannot test is close to one you cannot use. These are
`areev anonymize test` fixture files: a policy, the strings it **must** redact,
and the strings it **must not**. The run exits non-zero on either kind of
failure, so it belongs in CI.

```bash
areev anonymize test --fixtures examples/anonymize/india-identifiers.json
# → 26 fixtures: 26 passed, 0 failed (0 missed, 0 false positive)
examples/anonymize/smoke.sh     # the same, as the keyless smoke
```

## `india-identifiers.json`

Indian personal identifiers as citizens actually type them, mostly inside
Tamil prose (#418): bare and grouped mobile numbers (`9843122760`,
`98431 22760`, `+91 98431 22760`, `098431-22760`), Aadhaar numbers (Verhoeff
checksum), voter IDs (EPIC, cue-gated), and vehicle registration marks
(`TN 37 AB 1234`).

The `must_not_redact` half is the load-bearing one. It pins:

- `Aadhaar 2345 6789 0123` — Aadhaar-shaped, one digit off a valid number, so
  the Verhoeff check refuses it (the issue's own reproduction string; the
  valid check digit is `4`);
- day-first dates in every common separator (`12-05-2024`, `12.05.2024`,
  `12/05/2024`) — the dashed form used to be reported as a phone number;
- six-digit pincodes, including `pincode 641001` — `pincode` used to cue the
  PIN (secret) detector;
- rupee amounts, including a ten-digit one that starts 6–9 like a mobile;
- an uncued bare twelve-digit payment reference (UPI UTR) — bare Aadhaar runs
  need a cue, because one random twelve-digit run in ten passes Verhoeff;
- EPIC-shaped order numbers and lowercase prose that only looks like a plate.

Copy the file and swap in your own policy (`--policy-file` overrides the one in
the file) to check a candidate policy before you `anonymize set` it.

Shapes Areev does not hard-code — a ward reference, an internal case number —
go in the policy as regex detectors, and belong in a fixture file like this:

```json
{"detectors": [{"category": "ward_ref", "pattern": "W[0-9]{3}-[0-9]{4}",
                "cue": ["ward", "வார்டு"]}]}
```
