# macOS release signing

`release.yml` signs and notarizes the macOS binaries when six repository
secrets are set. With none of them set, the release still ships, unsigned, and
the macOS jobs log an "Unsigned macOS build" warning. Setting only some of them
fails the job.

The binary is a bare CLI executable, so it can't be stapled. Gatekeeper checks
the notarization ticket online the first time it runs.

## One-time setup

Run these on the Mac that holds the `Developer ID Application: Xoder PR LLC
(RST67A4A6S)` identity. Every `gh secret set` reads its value from stdin, so
nothing lands in shell history or on screen.

### 1. Export the signing certificate

1. Open Keychain Access, choose the login keychain, then My Certificates.
2. Expand `Developer ID Application: Xoder PR LLC (RST67A4A6S)` and confirm a
   private key sits under it.
3. Right-click the certificate, choose Export, save as `developer-id.p12`, and
   set a strong export password.

### 2. Create an App Store Connect API key

1. In App Store Connect, go to Users and Access, then Integrations, then Team Keys.
2. Generate a key with the Developer role.
3. Download `AuthKey_<KEYID>.p8`. Apple lets you download it only once.
4. Note the Key ID (on the key's row) and the Issuer ID (above the key list).

### 3. Store the secrets

```bash
cd U1F4E7
base64 -i developer-id.p12 | gh secret set MACOS_CERT_P12_BASE64
read -rs P && printf '%s' "$P" | gh secret set MACOS_CERT_PASSWORD; unset P
base64 -i AuthKey_<KEYID>.p8 | gh secret set APPLE_API_KEY_P8_BASE64
read -rs K && printf '%s' "$K" | gh secret set APPLE_API_KEY_ID; unset K
read -rs I && printf '%s' "$I" | gh secret set APPLE_API_ISSUER_ID; unset I
printf '%s' RST67A4A6S | gh secret set APPLE_TEAM_ID
gh secret list
```

Each `read -rs` waits silently for you to paste the value and press Enter.

### 4. Clean up and verify

1. Delete `developer-id.p12` and move `AuthKey_<KEYID>.p8` to a password
   manager.
2. Run the Release workflow by hand (Actions, then Release, then Run workflow).
   It creates a draft release. The macOS jobs should log `Notarization
   accepted`.
3. Download the draft's macOS tarball, extract it, and run
   `spctl -a -vvv -t install envelope`. Expect `accepted` and
   `source=Notarized Developer ID`. Then delete the draft release.

## Rotating

- Certificate renewed: redo steps 1 and 3 for `MACOS_CERT_P12_BASE64` and
  `MACOS_CERT_PASSWORD`.
- API key revoked: redo steps 2 and 3 for the three `APPLE_API_*` secrets.
