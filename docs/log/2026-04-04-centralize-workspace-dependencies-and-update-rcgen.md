# Centralize workspace dependencies and update rcgen

Lifted all dependencies (except CLI macOS target deps) to
`[workspace.dependencies]` so versions are managed in one place.
Crates reference them with `{ workspace = true }`, adding features
locally where needed.

Updated rcgen from 0.13 to 0.14: `CertificateParams::from_ca_cert_pem`
moved to `Issuer::from_ca_cert_pem`, and `signed_by` now takes an
`&Issuer` instead of separate cert + key args. Updated crossterm to
0.29.
