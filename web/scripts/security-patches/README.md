# Temporary dependency security backports

The registry has no patched release for `braces@3.0.3`
([GHSA-vfj7-8cjw-p6xm](https://github.com/advisories/GHSA-vfj7-8cjw-p6xm)) or
`deepmerge@4.3.1`
([GHSA-gcqx-w48m-w2h4](https://github.com/advisories/GHSA-gcqx-w48m-w2h4)) as of
2026-11-02. Nuxt still brings in these packages, so this project applies the
fixes below locally. Nuxt is updated to 4.6.3 and `node-forge` to 1.4.2;
the former Forge RSA backport is removed in favor of the upstream fix.

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

- `deepmerge.patch` follows the key-name denylist recommended in
  [upstream issue #273](https://github.com/TehShrike/deepmerge/issues/273).
  It rejects `__proto__`, `constructor`, and `prototype` in both target-copy and
  source-merge loops, including for null-prototype targets and nested values.
  This is a local mitigation, not an upstream release. Only `dist/cjs.js`, the
  package entrypoint used by Rollup's node resolver, is patched; direct imports
  of other package files bypass it. The upstream MIT license is included.
- The four `braces-*.patch` files implement the nesting guard recommended in
  [upstream issue #70](https://github.com/micromatch/braces/issues/70). The parser
  bounds brace and parenthesis nesting, and all recursive walkers reject depths
  over 100, including caller-supplied ASTs. Escaped/quoted braces remain literals.
  This is a local mitigation, not an upstream release. Patterns requiring more
  than 100 nested levels now throw a controlled `SyntaxError`. The upstream MIT
  license is included.
- `esbuild` is overridden to the published fixed version `0.28.2`; production
  build and typecheck verify compatibility with the installed Nuxt/Vite tooling.
- `glob` and `minimatch` are overridden to `14.0.2` and `11.0.1` to resolve
  [abort traversal](https://github.com/advisories/GHSA-3g3m-2fjr-6pw4) and
  [pattern amplification](https://github.com/advisories/GHSA-g3hv-75wg-m249).
  Regression tests exercise the actual archiver consumers' matching/exclusions
  and minimatch's preprocessing limit; typecheck and production build verify Nuxt.

These packages are build/development dependencies in this static frontend. The
Rust service serves generated files, and does not execute these Node packages.

`tests/security-dependencies.test.mjs` exercises prototype-poisoning attempts,
nested strings and ASTs, ordinary brace expansion, the real installed Nuxt
consumers, patch replay/tamper rejection, and RSA malformed DigestInfo encodings
with exponents 3 and 65537. RSA tests use
normal PKCS#1 padding and private-key-signed malformed encodings to test rejection;
they do not claim to demonstrate private-key-free forgery. Valid signatures with
and without NULL parameters and Node/OpenSSL SHA-256/384/512 signatures still pass.

The original package identities and versions remain intact. Consequently
`npm audit` still reports 7 low findings propagated from the `deepmerge` advisory;
it cannot detect local source patches. The dependency versions reported to scanners
remain unchanged; no fake package version is used. Dependabot triage must cite the
verified backport and prototype-poisoning regression tests. Replace the backports with patched releases
when available, then rerun the regression suite, typecheck, production build, and
audit.
