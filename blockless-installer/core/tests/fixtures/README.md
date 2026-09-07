# Fixtures

`vscode-update-api.darwin-universal.json` is a synthetic fixture matching the
known shape of `GET https://update.code.visualstudio.com/api/update/<platform>/stable/latest`
(`url`, `name`, `version`, `productVersion`, `hash`, `timestamp`, `sha256hash`,
`supportsFastUpdate`). It was built by hand, not captured from a live response,
because this sandbox has no route to `update.code.visualstudio.com`.

Per `/scope.md`, the real capture happens on the acceptance rig: replace this
file with an actual response body recorded there, then re-run
`live_response_still_matches_captured_shape` (normally `#[ignore]`d) to confirm
the live shape still parses. Only the fields `VscodeUpdateApiResponse` reads
(`url`, `sha256hash`, `productVersion`) need to be genuine; the rest are
present only so the fixture looks like a real payload.
