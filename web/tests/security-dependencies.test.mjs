import { test } from 'node:test';
import assert from 'node:assert/strict';
import { constants, createHash, generateKeyPairSync, privateEncrypt, sign } from 'node:crypto';
import { createRequire } from 'node:module';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import braces from 'braces';
import forge from 'node-forge';
import { applySecurityPatches } from '../scripts/security-patch.mjs';

test('braces bounds nesting through every string and AST entrypoint', () => {
  for (const [open, close] of [['{', '}'], ['(', ')']]) {
    const pattern = open.repeat(4000) + 'a,b' + close.repeat(4000);
    for (const invoke of [braces, braces.parse, braces.create, braces.compile, braces.expand, braces.stringify]) {
      assert.throws(() => invoke(pattern), { name: 'SyntaxError', message: /nesting exceeds/ });
    }
  }
  for (const invoke of [braces.compile, braces.expand, braces.stringify]) {
    const ast = { type: 'root', nodes: [] };
    let parent = ast;
    for (let i = 0; i < 4000; i++) {
      const child = { type: 'paren', nodes: [] };
      parent.nodes.push(child);
      parent = child;
    }
    assert.throws(() => invoke(ast), { name: 'SyntaxError', message: /nesting exceeds/ });
  }
  assert.deepEqual(braces.expand('a/{b,c}/{1..3}'), ['a/b/1', 'a/b/2', 'a/b/3', 'a/c/1', 'a/c/2', 'a/c/3']);
  assert.equal(braces.compile('a/{b,c}/d'), 'a/(b|c)/d');
  assert.equal(braces.stringify(braces.parse('a/{b,c}/d')), 'a/{b,c}/d');
  assert.equal(braces.stringify('"' + '{'.repeat(200) + '"'), '{'.repeat(200));
  assert.equal(braces.stringify('\\{'.repeat(200)), '{'.repeat(200));
  assert.equal(braces.stringify('{'.repeat(98) + 'a' + '}'.repeat(98)), '{'.repeat(98) + 'a' + '}'.repeat(98));
});

// Signed malformed encodings exercise verifier rejection; no padding checks are bypassed.
for (const publicExponent of [3, 65537]) test(`forge rejects noncanonical DigestInfo with exponent ${publicExponent}`, () => {
  const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048, publicExponent });
  const key = forge.pki.publicKeyFromPem(publicKey.export({ type: 'spki', format: 'pem' }));
  const message = Buffer.from('security regression');
  const digest = createHash('sha256').update(message).digest();
  const oid = Buffer.from('0609608648016503040201', 'hex');
  const nil = Buffer.from('0500', 'hex');
  const tlv = (tag, value, long = false) => Buffer.concat([Buffer.from(long ? [tag, 0x81, value.length] : [tag, value.length]), value]);
  const info = (parameters, innerLong = false, outerLong = false) => tlv(0x30, Buffer.concat([
    tlv(0x30, Buffer.concat([oid, parameters]), innerLong), tlv(4, digest),
  ]), outerLong);
  const signature = der => privateEncrypt({ key: privateKey, padding: constants.RSA_PKCS1_PADDING }, der).toString('binary');
  const verify = der => key.verify(digest.toString('binary'), signature(der));
  assert.equal(verify(info(nil)), true);
  assert.equal(verify(info(Buffer.alloc(0))), true);
  for (const der of [
    info(Buffer.from('0500020101', 'hex')),
    info(Buffer.from('040161', 'hex')),
    info(Buffer.from('0503616263', 'hex')),
    info(nil, true), info(nil, false, true),
  ]) assert.throws(() => verify(der), /valid RSASSA-PKCS1-v1_5 DigestInfo/);
  assert.equal(key.verify('wrong digest', signature(info(nil))), false);
  for (const algorithm of ['sha256', 'sha384', 'sha512']) {
    const hash = createHash(algorithm).update(message).digest('binary');
    assert.equal(key.verify(hash, sign(algorithm, message, privateKey).toString('binary')), true);
  }
  const pss = forge.pss.create({ md: forge.md.sha256.create(), mgf: forge.mgf.mgf1.create(forge.md.sha256.create()), saltLength: 32 });
  const pssSignature = sign('sha256', message, { key: privateKey, padding: constants.RSA_PKCS1_PSS_PADDING, saltLength: 32 });
  assert.equal(key.verify(digest.toString('binary'), pssSignature.toString('binary'), pss), true);
  assert.equal(key.verify(message.toString('binary'), signature(message), 'NONE'), true);
});

test('security backports are idempotent and reject changed sources and versions before writing', () => {
  const root = mkdtempSync(join(tmpdir(), 'kodrik-security-patches-'));
  try {
    const manifest = JSON.parse(readFileSync(new URL('../scripts/security-patches/manifest.json', import.meta.url)));
    const lock = { packages: {} };
    const originals = [];
    for (const patch of manifest) {
      const path = `node_modules/${patch.package}`;
      lock.packages[path] = { version: patch.version };
      mkdirSync(join(root, path, 'lib'), { recursive: true });
      writeFileSync(join(root, path, 'package.json'), JSON.stringify({ version: patch.version }));
      let source = readFileSync(new URL(`../${path}/${patch.file}`, import.meta.url), 'utf8');
      const diff = readFileSync(new URL(`../scripts/security-patches/${patch.patch}`, import.meta.url), 'utf8');
      for (const hunk of diff.split(/^@@[^\n]*\n/m).slice(1).reverse()) {
        const lines = hunk.split('\n').slice(0, -1);
        const old = lines.filter(line => /^[ -]/.test(line)).map(line => line.slice(1)).join('\n') + '\n';
        const patched = lines.filter(line => /^[ +]/.test(line)).map(line => line.slice(1)).join('\n') + '\n';
        source = source.replace(patched, () => old);
      }
      assert.equal(createHash('sha256').update(source).digest('hex'), patch.original);
      const file = join(root, path, patch.file);
      originals.push([file, source]);
      writeFileSync(file, source);
    }
    const lockFile = join(root, 'package-lock.json');
    writeFileSync(lockFile, JSON.stringify(lock));
    assert.equal(applySecurityPatches(root), manifest.length);
    assert.equal(applySecurityPatches(root), 0);
    const file = join(root, 'node_modules/braces/lib/parse.js');
    writeFileSync(...originals[0]);
    writeFileSync(file, 'changed');
    assert.throws(() => applySecurityPatches(root), /unexpected source/);
    assert.equal(readFileSync(file, 'utf8'), 'changed');
    assert.equal(readFileSync(originals[0][0], 'utf8'), originals[0][1]);
    lock.packages['node_modules/node-forge'].version = '1.4.1';
    writeFileSync(lockFile, JSON.stringify(lock));
    assert.throws(() => applySecurityPatches(root), /lock version/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('Nuxt consumers resolve the verified dependency implementations', () => {
  const require = createRequire(import.meta.url);
  const listhen = createRequire(require.resolve('listhen'));
  const micromatch = createRequire(require.resolve('micromatch'));
  assert.equal(listhen.resolve('node-forge'), require.resolve('node-forge'));
  assert.equal(micromatch.resolve('braces'), require.resolve('braces'));
});
