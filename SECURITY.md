# Security

VMherd holds API tokens and types into root consoles, so security reports are welcome.

Please report vulnerabilities privately through GitHub: **Security → Report a vulnerability** on
<https://github.com/WietseWind/VMherd>. Do not open a public issue for them.

Supply chain: every change and a daily job run `cargo-deny` (RustSec advisories, crate sources,
licenses); Dependabot proposes dependency and GitHub Actions updates; builds use the committed
`Cargo.lock`; third-party actions are pinned to commit SHAs.
