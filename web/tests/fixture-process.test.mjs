import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { startFixture, stopFixture } from './fixture-process.mjs';

const helper = new URL('./fixture-process.mjs', import.meta.url).href;
const cwd = new URL('../../', import.meta.url);
const fake = `console.log('CHAT_TEST_URL=http://127.0.0.1:1234/'); setInterval(()=>{},1000)`;

function alive(pid) {
  try { process.kill(pid, 0); return true; } catch (error) {
    if (error.code === 'ESRCH') return false;
    throw error;
  }
}

async function waitUntil(check, message, timeout = 5000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await delay(50);
  }
  assert.fail(message);
}

for (const signal of ['exit', 'SIGTERM', 'SIGINT']) {
  test(`controller ${signal} cleans up its detached fixture`, async () => {
    const controller = spawn(process.execPath, ['--input-type=module', '-e', `
      import { startFixture } from ${JSON.stringify(helper)};
      const {server} = await startFixture({command:process.execPath,args:['-e',${JSON.stringify(fake)}]});
      process.send(server.pid);
      process.on('message',()=>process.exit(0));
    `], { stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
    let pid;
    try {
      [pid] = await once(controller, 'message');
      assert.ok(alive(pid));
      const exited = once(controller, 'exit');
      if (signal === 'exit') controller.send('exit');
      else controller.kill(signal);
      await exited;
      await waitUntil(() => !alive(pid), `fixture ${pid} survived ${signal}`);
    } finally {
      controller.kill('SIGKILL');
      if (pid) { try { process.kill(-pid, 'SIGKILL'); } catch {} }
    }
  });
}

test('startup timeout kills the detached fixture', async () => {
  // A file records the PID even though this fixture never announces readiness.
  const { mkdtemp, readFile, rm } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const dir = await mkdtemp(join(tmpdir(), 'fixture-timeout-'));
  let pid;
  try {
    await assert.rejects(startFixture({ command: process.execPath, args: ['-e', `
      require('node:fs').writeFileSync(${JSON.stringify(join(dir, 'pid'))},String(process.pid));
      setInterval(()=>{},1000);
    `], startupTimeout: 500 }), /startup timed out/);
    pid = Number(await readFile(join(dir, 'pid'), 'utf8'));
    assert.ok(!alive(pid));
  } finally {
    if (pid) { try { process.kill(-pid, 'SIGKILL'); } catch {} }
    await rm(dir, { recursive: true, force: true });
  }
});

test('spawn errors reject startup', async () => {
  await assert.rejects(startFixture({command:'/no-such-kodrik-fixture'}), /ENOENT/);
});

test('early fixture exit rejects startup with diagnostics', async () => {
  await assert.rejects(startFixture({ command: process.execPath, args: ['-e',
    "console.error('fixture failed');process.exit(7)"] }), /Fixture exited 7:[\s\S]*fixture failed/);
});

test('shutdown escalates when the fixture ignores SIGTERM and can be repeated', async () => {
  const { server } = await startFixture({ command: process.execPath, args: ['-e', `
    process.on('SIGTERM',()=>{}); ${fake}
  `] });
  await stopFixture(server, 100);
  assert.equal(server.signalCode, 'SIGKILL');
  await stopFixture(server, 100);
});

test('dead fixtures do not hang shutdown', async () => {
  const { server } = await startFixture({ command: process.execPath, args: ['-e', fake] });
  const exited = once(server, 'exit');
  process.kill(-server.pid, 'SIGKILL');
  await exited;
  await stopFixture(server, 100);
});

test('SIGKILL of the controller stops the real Rust browser fixture (and cargo when used)', { timeout: 90000 }, async () => {
  // A prebuilt test binary avoids Cargo's build lock while another suite compiles.
  const fixtureOptions = process.env.CHAT_TEST_BINARY
    ? { command: process.env.CHAT_TEST_BINARY, args: ['web::chat_tests::browser_fixture', '--ignored', '--nocapture'] }
    : {};
  const controller = spawn(process.execPath, ['--input-type=module', '-e', `
    import { startFixture } from ${JSON.stringify(helper)};
    const { server, url } = await startFixture({cwd:${JSON.stringify(cwd.pathname)},...${JSON.stringify(fixtureOptions)}});
    process.send({pid:server.pid,url});
    setInterval(()=>{},1000);
  `], { stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
  let pid, descendants = [], output = '';
  controller.stderr.on('data', data => { output += data; });
  try {
    const ready = await Promise.race([
      once(controller, 'message').then(([message]) => message),
      once(controller, 'exit').then(([code]) => { throw new Error(`controller exited ${code}: ${output}`); }),
    ]);
    pid = ready.pid;
    // Both cargo and its Rust test process must disappear, not just the HTTP listener.
    const processes = execFileSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' })
      .trim().split('\n').map(line => line.trim().split(/\s+/).map(Number));
    descendants = processes.filter(([, parent]) => parent === pid).map(([child]) => child);
    if (!process.env.CHAT_TEST_BINARY) assert.ok(descendants.length, 'cargo must have a running Rust fixture child');
    assert.ok((await fetch(ready.url)).status < 500);
    const exited = once(controller, 'exit');
    controller.kill('SIGKILL');
    await exited;
    await waitUntil(() => [pid, ...descendants].every(pid => !alive(pid)),
      `cargo/fixture survived controller SIGKILL: ${[pid, ...descendants]}`);
    await assert.rejects(fetch(ready.url));
  } finally {
    controller.kill('SIGKILL');
    if (pid) { try { process.kill(-pid, 'SIGKILL'); } catch {} }
  }
});
