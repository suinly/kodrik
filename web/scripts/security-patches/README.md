# Temporary dependency security backports

The registry has no patched release for `braces@3.0.3`
([GHSA-vfj7-8cjw-p6xm](https://github.com/advisories/GHSA-vfj7-8cjw-p6xm)) or
`node-forge@1.4.0`
([GHSA-86w9-cpqp-85rv](https://github.com/advisories/GHSA-86w9-cpqp-85rv)) as of
2026-10-26. Nuxt is updated to 4.6.2 and `serialize-javascript` to 7.1.4 to
resolve their published advisories. Nuxt still brings in the packages above,
so this project applies the fixes below locally.

`npm install`/`npm ci` applies the backports through `postinstall`. The `dev`,
`build`, `typecheck`, and `test` npm commands verify them again before running,
including after an install with lifecycle scripts disabled. Direct `npx nuxt`
invocations bypass these hooks; use the package scripts or run
`npm run security:patch` first.

The patcher checks package versions and complete original/patched SHA-256 hashes
for every matching lockfile installation, including nested copies. An unexpected
version, missing file, changed source, or incorrect patch result stops the command
before any source is written. Running it again is idempotent. A dependency upgrade
must review and remove/replace the relevant patch and manifest entry.

## Sources and limits

- `node-forge.patch` is the unchanged `lib/rsa.js` portion of upstream
  [PR #1159](https://github.com/digitalbazaar/forge/pull/1159), commit
  [`db05494352dd3c9a5a07c5270333dcc78866cad2`](https://github.com/digitalbazaar/forge/commit/db05494352dd3c9a5a07c5270333dcc78866cad2),
  authored by Matthew Conroy. It requires canonical DigestInfo encoding, with or
  without NULL parameters. Element-count-only fixes leave NULL-content and
  nonminimal-length bypasses. This patch is not yet merged or released upstream.
  The supplied BSD license covers the backport. Only the Node implementation used
  by Nuxt's `listhen` is patched; Forge's prebuilt browser bundles are not used or
  modified here.
- The four `braces-*.patch` files implement the nesting guard recommended in
  [upstream issue #70](https://github.com/micromatch/braces/issues/70). The parser
  bounds brace and parenthesis nesting, and all recursive walkers reject depths
  over 100, including caller-supplied ASTs. Escaped/quoted braces remain literals.
  This is a local mitigation, not an upstream release. Patterns requiring more
  than 100 nested levels now throw a controlled `SyntaxError`. The upstream MIT
  license is included.
- `esbuild` is overridden to the published fixed version `0.28.2`; production
  build and typecheck verify compatibility with the installed Nuxt/Vite tooling.

These packages are build/development dependencies in this static frontend. The
Rust service serves generated files, and does not execute these Node packages.

`tests/security-dependencies.test.mjs` exercises nested strings and ASTs, ordinary
brace expansion, the real installed Nuxt consumers, patch replay/tamper rejection,
and RSA malformed DigestInfo encodings with exponents 3 and 65537. RSA tests use
normal PKCS#1 padding and private-key-signed malformed encodings to test rejection;
they do not claim to demonstrate private-key-free forgery. Valid signatures with
and without NULL parameters and Node/OpenSSL SHA-256/384/512 signatures still pass.

The original package identities and versions remain intact. Consequently
`npm audit` still reports 7 high findings propagated from the `node-forge` advisory;
it cannot detect local source patches. The dependency versions reported to scanners
remain unchanged; no fake package version is used. Dependabot triage must cite the
verified backport and RSA regression tests. Replace the backports with patched releases
when available, then rerun the regression suite, typecheck, production build, and
audit.
