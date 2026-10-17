# Contributing to evo-esp32

Thank you for your interest in contributing to evo-esp32. This document
explains the licensing terms contributions are accepted under, the
contribution flow, and the basic expectations for contributed code.

## Licensing

This repository ships the embedded fabric under the **Business Source
License 1.1** (see `LICENSE`) and the surface others implement against
(slot contracts, SDK types, example plugins) under the **Apache License,
Version 2.0** (see the `LICENSE` file in each such crate's directory).
A crate is Business Source License 1.1 unless its directory says
otherwise.

When you contribute code to this repository, your contribution is
accepted under the licence that applies to the file or directory
you are modifying. If you are adding a new file, the licence is
determined by the directory it lives in (workspace default Business
Source License 1.1; Apache License 2.0 for crates that override the
workspace licence).

The framework licence converts to Apache 2.0 four years after each
release per the LICENSE file's Change Date and Change License
parameters.

## Developer Certificate of Origin (DCO)

Every commit submitted to this project must carry a `Signed-off-by`
line attesting that the contributor has the right to contribute the
code under the project's licence. This is the **Developer
Certificate of Origin** (https://developercertificate.org/), the
same mechanism the Linux kernel and many other major open-source
projects use.

The DCO is a developer's affirmation that they wrote the
contribution or otherwise have the right to submit it under the
project's licence. The full text of the DCO is reproduced at the
end of this document for reference.

To sign off on a commit, add the `-s` flag to `git commit`:

```bash
git commit -s -m "your commit message"
```

This appends a line of the form:

```text
Signed-off-by: Your Name <your.email@example.com>
```

If you forget to sign off on a commit, you can amend it with:

```bash
git commit --amend -s
```

If the commit has already been pushed and you need to retroactively
sign off, you may need to interactively rebase and sign off the
relevant commits.

Continuous integration enforces the sign-off requirement on every
pull request and on direct pushes to the main branch.

## Contribution flow

1. Fork the repository or create a branch from `main` if you have
   write access.
2. Implement your change. Follow the existing patterns in the
   relevant crate or directory; if you are adding a new public
   surface, document its behaviour with rustdoc.
3. Run the compulsory cargo workout before every commit (and
   again before opening a pull request):
   `scripts/check.sh` (once the workspace exists). That is
   `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets --locked -- -D warnings`,
   `cargo test --workspace --locked`, and
   `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked`,
   then every firmware crate built for its device.
   rustdoc intra-doc links are part of the gate. Do not `#[allow]`
   rustdoc or clippy to silence it.
4. Commit your change with a `Signed-off-by` line (`git commit -s`).
5. Open a pull request describing the change, the motivation, and
   any decisions worth highlighting to a reviewer.

## Code of conduct

Engagement with this project is expected to be courteous,
constructive, and focused on the engineering work. Behaviour that
creates a hostile environment for contributors will be addressed.

## Reporting security issues

Please do not report security issues through public channels. Open
a private channel by emailing the maintainer (contact details on
the repository's main README) or by opening a private security
advisory through the repository's security tab if available. The
maintainer will respond within a reasonable time and coordinate
disclosure as appropriate.

## Developer Certificate of Origin (full text)

```text
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of
this license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including
    all personal information I submit with it, including my
    sign-off) is maintained indefinitely and may be redistributed
    consistent with this project or the open source license(s)
    involved.
```
