import { spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const fixtures = new Set();

function killGroup(server, signal) {
  if (!server.pid) return;
  try { process.kill(-server.pid, signal); } catch (error) {
    if (error.code !== 'ESRCH') throw error;
  }
}

process.on('exit', () => {
  for (const server of fixtures) killGroup(server, 'SIGKILL');
});
for (const [signal, code] of [['SIGINT', 130], ['SIGTERM', 143]]) {
  process.once(signal, () => process.exit(code));
}

export async function stopFixture(server, timeout = 2000) {
  if (!server?.pid) return;
  // Cargo may have exited before its descendants; always address the group.
  killGroup(server, 'SIGTERM');
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    try { process.kill(-server.pid, 0); } catch (error) {
      if (error.code !== 'ESRCH') throw error;
      fixtures.delete(server);
      return;
    }
    await delay(25);
  }
  killGroup(server, 'SIGKILL');
  // Reap our direct child without depending on an exit event that may have fired.
  const reapDeadline = Date.now() + 1000;
  while (server.exitCode === null && server.signalCode === null) {
    if (Date.now() >= reapDeadline) throw new Error(`Fixture ${server.pid} did not exit after SIGKILL`);
    await delay(10);
  }
  fixtures.delete(server);
}

export async function startFixture({
  storage, listen,
  command = 'cargo',
  args = ['test', 'web::chat_tests::browser_fixture', '--', '--ignored', '--nocapture'],
  cwd = fileURLToPath(new URL('../../', import.meta.url)),
  startupTimeout = 60000,
} = {}) {
  const server = spawn(command, args, {
    cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, CHAT_TEST_PARENT_PID: String(process.pid),
      ...(storage ? { CHAT_TEST_DATA: storage } : {}),
      ...(listen ? { CHAT_TEST_LISTEN: listen } : {}) },
  });
  fixtures.add(server);
  let timer, output = '';
  try {
    const url = await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error(`Fixture startup timed out:\n${output}`)), startupTimeout);
      for (const stream of [server.stdout, server.stderr]) stream.on('data', data => {
        output = (output + data).slice(-65536);
        const match = output.match(/CHAT_TEST_URL=(http:\/\/\S+)/);
        if (match) resolve(match[1]);
      });
      server.once('error', reject);
      server.once('exit', (code, signal) => reject(new Error(`Fixture exited ${signal ?? code}:\n${output}`)));
    });
    return { server, url };
  } catch (error) {
    await stopFixture(server);
    fixtures.delete(server);
    throw error;
  } finally {
    clearTimeout(timer);
  }
}
