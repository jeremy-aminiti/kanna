# OpenPGP.js bundled with kd

The apt signature adapter uses OpenPGP.js **6.3.1**, licensed under the GNU
Lesser General Public License version 3 or later (`LGPL-3.0+`). The upstream
LICENSE and the unmodified distributed Node source, including embedded
dependency notices, accompany the built module in `licenses/openpgp/`.
The accompanying GPL v3 text (incorporated by LGPL v3) comes from
https://github.com/spdx/license-list-data/blob/v3.27.0/text/GPL-3.0-or-later.txt.
Preserve that directory, emitted legal notices and source maps when distributing
the module. kd's TypeScript sources and tsup configuration provide the inputs
for rebuilding the combined module with a modified library.

- Upstream: https://github.com/openpgpjs/openpgpjs
- Release: https://github.com/openpgpjs/openpgpjs/releases/tag/v6.3.1
- Source commit: `2ac0048404b74a3595d503125b53f3b3d0486bec`
- Corresponding upstream source: https://github.com/openpgpjs/openpgpjs/tree/2ac0048404b74a3595d503125b53f3b3d0486bec
- Published package: https://registry.npmjs.org/openpgp/-/openpgp-6.3.1.tgz
- Package SHA-256: `6a333c1880fecf274f01f69dd7157afc7dd03b81fb586f36e57a61f9cbfe6e10`
- Package SHA-512 integrity is pinned in the repository's `pnpm-lock.yaml`.

The copied source is the published Node distribution, not a claim that kd
rebuilt upstream OpenPGP.js from its original build sources. That distribution
includes third-party implementations and their notices; it has no mandatory
npm runtime dependencies. The adapter deliberately supports v4 RSA keys of at
least 3072 bits and SHA512 signatures. Other profiles, including optional
curve-specific host dependencies, are not enabled.

This adapter does not discover keys or provision credentials. It processes only
explicitly supplied keys and fingerprints. Ubuntu apt/GPG interoperability,
real-key custody on the owner's release host, and release-command integration
remain separate acceptance work.

Bundle validation (using an already installed Node with `registerHooks`, such
as Node 24): from the repository root, build with
`pnpm --dir tools/kd exec tsup --out-dir ../../.tmp/apt-adapter-bundle`, then run
`node tools/kd/tests/apt-bundle-smoke.mjs .tmp/apt-adapter-bundle`.
The smoke test fences both ESM and CommonJS resolution to emitted files and
Node built-ins, and checks source/license metadata. Test keys stay in memory.
