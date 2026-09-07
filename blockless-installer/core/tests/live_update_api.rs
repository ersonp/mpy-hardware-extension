//! Rig-only: proves the real update API's response still parses as
//! `VscodeUpdateApiResponse` -- i.e. still matches the shape
//! `tests/fixtures/vscode-update-api.darwin-universal.SHAPE.json` stands in
//! for (that fixture is HAND-AUTHORED, not a recorded response; see
//! `tests/fixtures/README.md`). Never run in CI or the sandbox -- both lack
//! a route to `update.code.visualstudio.com` (see `/scope.md`: "No live
//! third-party endpoints in CI").
//!
//! Running this on the rig is also step 1 of replacing the shape fixture
//! with a real capture: save this test's `body` as
//! `tests/fixtures/vscode-update-api.darwin-universal.json` (drop the
//! `.SHAPE.` marker), `git rm` the `.SHAPE.` file, and update
//! `core/src/manifest.rs`'s fixture constant + test to match. See
//! `tests/fixtures/README.md` for the full checklist.

use blockless_installer_core::manifest::VscodeUpdateApiResponse;

#[test]
#[ignore = "hits the real VS Code update API; run explicitly on the acceptance rig"]
fn live_response_still_matches_captured_shape() {
    let body = reqwest::blocking::get(
        "https://update.code.visualstudio.com/api/update/darwin-universal/stable/latest",
    )
    .expect("request to the VS Code update API failed")
    .text()
    .expect("could not read the response body");
    VscodeUpdateApiResponse::parse(&body)
        .expect("live response no longer matches the hand-authored shape fixture");
}
