# Fixtures

`vscode-update-api.darwin-universal.SHAPE.json` is a HAND-AUTHORED SHAPE
STAND-IN, not a captured response -- the `.SHAPE.` in its name is there so it
can never be mistaken for one. It matches the known shape of
`GET https://update.code.visualstudio.com/api/update/<platform>/stable/latest`
(`url`, `name`, `version`, `productVersion`, `hash`, `timestamp`, `sha256hash`,
`supportsFastUpdate`) but nothing in it was observed from a real response --
this sandbox has no route to `update.code.visualstudio.com`. Its `sha256hash`
is deliberately all-zeros (`_fixtureNote` says so too) rather than a
plausible-looking 64-hex string, specifically so nobody downstream mistakes it
for a real digest.

Per `/scope.md`, the real capture happens on the acceptance rig -- this is a
required acceptance step, not optional cleanup:

1. On the rig, `curl https://update.code.visualstudio.com/api/update/darwin-universal/stable/latest`
   (or let `live_response_still_matches_captured_shape` fetch it) and save the
   raw response body.
2. Record it as `vscode-update-api.darwin-universal.json` (drop the `.SHAPE.`
   marker -- that name means "this one is real").
3. `git rm vscode-update-api.darwin-universal.SHAPE.json`.
4. Update `core/src/manifest.rs`'s `include_str!` path and the fixture-loading
   test's name/doc comment to match (they currently say "hand-authored shape
   stand-in"; once real, they should say "captured on the rig on <date>").
5. Run `cargo test --workspace -- --ignored live_response_still_matches_captured_shape`
   to confirm the live shape still parses against the newly recorded fixture.

Only the fields `VscodeUpdateApiResponse` reads (`url`, `sha256hash`,
`productVersion`) need to be genuine; the rest exist only so the fixture looks
like a real payload.
