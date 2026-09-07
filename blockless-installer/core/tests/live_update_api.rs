//! Rig-only: proves the captured fixture (`tests/fixtures/vscode-update-api.darwin-universal.json`)
//! still matches what the real update API returns. Never run in CI or the
//! sandbox -- both lack a route to `update.code.visualstudio.com` (see
//! `/scope.md`: "No live third-party endpoints in CI").

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
        .expect("live response no longer matches the captured fixture shape");
}
