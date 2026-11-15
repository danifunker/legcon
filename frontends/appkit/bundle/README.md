# legcon.app

`build-app.sh` builds the AppKit shell and makes `legcon.app` (this folder's `Info.plist` and
`legcon.icns`); with `--universal` the binary holds Apple silicon and Intel, and with
`--package VERSION` it also leaves `legcon-macos-<arch>-VERSION.zip` and `.dmg`. It refuses a
binary that links anything macOS does not ship (legcon is pure Rust, D19).

`make-icon.sh` redraws the icon from `make-icon.swift`; both outputs are committed.

## Signing and notarization

The pipeline's `build` job (`.github/workflows/ci.yml`) signs and notarizes when these repository
secrets exist, and signs ad hoc with a warning when they do not. The names are vintrdp's (and
rusty-backup's and iris's), so one secret population script fills all of them:

| Secret | What it is |
|---|---|
| `MACOS_CERTIFICATE` | base64 of a .p12 holding the **Developer ID Application** certificate and its private key |
| `MACOS_CERTIFICATE_PWD` | the .p12 password |
| `MACOS_TEAM_ID` | Apple team ID |
| `MACOS_NOTARIZE_API_KEY`, `MACOS_NOTARIZE_API_KEY_ID`, `MACOS_NOTARIZE_API_ISSUER` | App Store Connect API key (base64 of the .p8), its key ID and the issuer ID |
| `MACOS_NOTARIZE_APPLE_ID`, `MACOS_NOTARIZE_PASSWORD` | instead of the API key: Apple ID and app-specific password |

Locally, `build-app.sh` reads `MACOS_SIGN_IDENTITY` and either
`NOTARY_KEY_ID` / `NOTARY_ISSUER_ID` / `NOTARY_KEY_PATH` or
`NOTARY_APPLE_ID` / `NOTARY_PASSWORD` / `NOTARY_TEAM_ID`, as vintrdp's `package-macos.sh` does.
