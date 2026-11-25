// Interrupt the packaged process during a real NTFS write, using only temporary test files.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const executable = path.resolve(process.argv[2]);
const working = path.join(project, '.timecut-build');
fs.mkdirSync(working, { recursive: true });
const test = fs.mkdtempSync(path.join(working, 'recovery-qa-'));
const source = path.join(test, '测试手机'),
  archive = path.join(test, '图库');
fs.mkdirSync(source);
fs.mkdirSync(archive);
const env = { ...process.env, TIMECUT_DATA_DIR: path.join(test, '用户数据') };
const photo = path.join(source, 'A.png'),
  movie = path.join(source, 'B.mp4');
const args = ['--import-folder', source, archive];
const sha = async (file) => {
  const digest = createHash('sha256');
  for await (const chunk of fs.createReadStream(file)) digest.update(chunk);
  return digest.digest('hex');
};
const visibleFiles = (dir) =>
  fs
    .readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.name !== '.time-cut')
    .flatMap((entry) => {
      const file = path.join(dir, entry.name);
      return entry.isDirectory() ? visibleFiles(file) : [file];
    });
const report = { test, executable };
const stage = path.join(archive, '.time-cut', 'staging');
let child;
try {
  fs.copyFileSync(path.join(project, 'src-tauri', 'icons', 'icon.png'), photo);
  const ffmpeg = path.join(path.dirname(executable), 'ffmpeg', 'ffmpeg-x86_64-pc-windows-msvc.exe');
  const generate = spawnSync(
    ffmpeg,
    ['-v', 'error', '-f', 'lavfi', '-i', 'color=s=128x96:d=0.2', '-c:v', 'libx264', movie],
    { windowsHide: true, encoding: 'utf8' },
  );
  assert.equal(generate.status, 0, generate.stderr);
  // MP4 readers skip the free-space box. A large valid file makes the mid-copy interruption observable.
  const boxSize = 384 * 1024 * 1024;
  const descriptor = fs.openSync(movie, 'r+');
  const start = fs.fstatSync(descriptor).size;
  const header = Buffer.alloc(8);
  header.writeUInt32BE(boxSize);
  header.write('free', 4, 'ascii');
  fs.writeSync(descriptor, header, 0, 8, start);
  fs.ftruncateSync(descriptor, start + boxSize);
  fs.closeSync(descriptor);
  const size = fs.statSync(movie).size;
  child = spawn(executable, args, { env, windowsHide: true, stdio: 'ignore' });
  const terminated = once(child, 'exit');
  const deadline = Date.now() + 60000;
  let interruptedBytes;
  while (Date.now() < deadline && child.exitCode === null && !interruptedBytes) {
    for (const entry of fs.existsSync(stage) ? fs.readdirSync(stage) : []) {
      if (!entry.endsWith('.mp4')) continue;
      let handle;
      try {
        handle = fs.openSync(path.join(stage, entry), 'r');
        const observed = fs.fstatSync(handle).size;
        if (observed > 0 && observed < size && child.kill()) interruptedBytes = observed;
      } catch {
      } finally {
        if (handle !== undefined) fs.closeSync(handle);
      }
    }
    await delay(2);
  }
  assert.ok(interruptedBytes, 'failed to observe a partial transfer');
  await terminated;
  assert.deepEqual(
    visibleFiles(archive).map((file) => path.basename(file)),
    ['A.png'],
  );
  assert.ok(fs.readdirSync(stage).some((name) => name.endsWith('.mp4')));
  const run = () => {
    const result = spawnSync(executable, args, {
      env,
      windowsHide: true,
      encoding: 'utf8',
      timeout: 180000,
    });
    assert.equal(result.status, 0, result.error?.message || result.stderr || result.stdout);
    return JSON.parse(result.stdout.trim());
  };
  const resumed = run(),
    repeat = run();
  assert.deepEqual([resumed.added, resumed.skipped, resumed.failed], [1, 1, 0]);
  assert.deepEqual([repeat.added, repeat.skipped, repeat.failed], [0, 2, 0]);
  const files = visibleFiles(archive);
  assert.equal(files.length, 2);
  assert.equal(fs.readdirSync(stage).length, 0);
  const copied = files.find((file) => path.extname(file) === '.mp4');
  const verifiedHash = await sha(movie);
  assert.equal(await sha(copied), verifiedHash);
  assert.equal(await sha(files.find((f) => f.endsWith('.png'))), await sha(photo));
  Object.assign(report, {
    size,
    interruptedBytes,
    resumed,
    repeat,
    sha256: verifiedHash,
    partialPublished: false,
    stagingAfterRecovery: 0,
  });
  // Keep the result and small fixtures. Release the two large disposable copies.
  fs.unlinkSync(movie);
  fs.unlinkSync(copied);
} catch (error) {
  report.error = String(error.stack || error);
  console.error(error);
  process.exitCode = 1;
} finally {
  if (child && child.exitCode === null && !child.killed) child.kill();
  const result = path.join(test, 'result.json');
  fs.writeFileSync(result, JSON.stringify(report, null, 2));
  console.log(result);
}
