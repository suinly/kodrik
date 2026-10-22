import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const directory = new URL('./security-patches/', import.meta.url);
const patches = JSON.parse(readFileSync(new URL('manifest.json', directory), 'utf8'));
const sha256 = source => createHash('sha256').update(source).digest('hex');

export function applySecurityPatches(root = fileURLToPath(new URL('../', import.meta.url))) {
  const lock = JSON.parse(readFileSync(resolve(root, 'package-lock.json'), 'utf8'));
  const writes = [];
  for (const patch of patches) {
    const copies = Object.entries(lock.packages).filter(([path]) => path.endsWith(`node_modules/${patch.package}`));
    assert.ok(copies.length, `Review security backport: ${patch.package} missing from lockfile`);
    for (const [path, locked] of copies) {
      const installed = JSON.parse(readFileSync(resolve(root, path, 'package.json'), 'utf8'));
      assert.equal(locked.version, patch.version, `Review security backport: ${path} lock version`);
      assert.equal(installed.version, patch.version, `Review security backport: ${path} installed version`);
      const file = resolve(root, path, patch.file);
      let source = readFileSync(file, 'utf8');
      if (sha256(source) === patch.patched) continue;
      assert.equal(sha256(source), patch.original, `Review security backport: unexpected source ${file}`);
      const diff = readFileSync(new URL(patch.patch, directory), 'utf8');
      for (const hunk of diff.split(/^@@[^\n]*\n/m).slice(1)) {
        const lines = hunk.split('\n').slice(0, -1);
        const before = lines.filter(line => /^[ -]/.test(line)).map(line => line.slice(1)).join('\n') + '\n';
        const after = lines.filter(line => /^[ +]/.test(line)).map(line => line.slice(1)).join('\n') + '\n';
        assert.equal(source.split(before).length, 2, `Security patch context mismatch: ${file}`);
        source = source.replace(before, () => after);
      }
      assert.equal(sha256(source), patch.patched, `Security patch result mismatch: ${file}`);
      writes.push([file, source]);
    }
  }
  // Validate every copy before changing any installed source.
  for (const [file, source] of writes) writeFileSync(file, source);
  return writes.length;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  console.log(`Security backports verified (${applySecurityPatches()} files patched)`);
}
