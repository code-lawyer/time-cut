// Packaged Windows USB acceptance for a connected, authorized iPhone or selected Android phone.
// Originals and the isolated profile are kept locally. The report contains counts only.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import { verifyLocalGallery, verifyOriginals } from './verify_timecut_archive.mjs';
import { compareGalleryRun, galleryInbox, galleryScope, loadGallerySnapshot, observePhoneProgress } from './verify_timecut_android.mjs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { setTimeout as delay } from 'node:timers/promises';

const androidFlag = process.argv.indexOf('--android-device');
const android = androidFlag >= 0;
const deviceName = android ? process.argv[androidFlag + 1] : null;
if (android) assert.ok(deviceName, 'An exact Android device name is required');
const galleryFlag = process.argv.indexOf('--gallery-snapshot');
if (galleryFlag >= 0) assert.ok(android && process.argv[galleryFlag + 1], 'Android gallery snapshot path required');
const gallerySnapshot = galleryFlag >= 0 ? loadGallerySnapshot(process.argv[galleryFlag + 1]) : null;
const singleFlag = process.argv.indexOf('--single-scan');
const single = singleFlag >= 0;
const scenarioName = single ? process.argv[singleFlag + 1] : null;
if (single) assert.ok(android && typeof scenarioName === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,79}$/.test(scenarioName),
  'Single scan requires an exact Android device and an alphanumeric scenario name');
const prefix = android ? 'android-full-' : 'iphone-full-';
const args = process.argv.slice(2, android ? androidFlag : process.argv.length);
const executable = path.resolve(args[0]);
const testRoot = path.resolve('.timecut-build');
const verifyLocal = args[2] === '--verify-local';
if (single) assert.ok(args[1] && !verifyLocal, 'Single scan requires an existing Android acceptance root');
assert.ok(!verifyLocal || args[1]);
const root = args[1] ? path.resolve(args[1]) : fs.mkdtempSync(path.join(testRoot, prefix));
assert.ok(path.dirname(root) === testRoot && path.basename(root).startsWith(prefix));
const archive = path.join(root, 'originals');
const profile = path.join(root, 'profile');
if (verifyLocal) {
  const totals = verifyLocalGallery(executable, archive, profile);
  fs.writeFileSync(path.join(root, 'local-verification.json'), JSON.stringify(totals, null, 2));
  console.log(JSON.stringify(totals));
  process.exit(0);
}
const galleryResult = path.join(root, 'gallery-result.json');
const resuming = fs.existsSync(path.join(root, 'result.json')) || (gallerySnapshot && fs.existsSync(galleryResult));
if (single) assert.ok(resuming, 'The previous phone acceptance record is missing');
if (args[1] && !resuming) {
  const baseline = JSON.parse(fs.readFileSync(path.join(root, 'baseline.json'), 'utf8'));
  assert.equal(baseline.state, 'waiting_for_user_to_connect');
  assert.equal(baseline.importStarted, false);
  for (const folder of [archive, profile]) assert.equal(fs.readdirSync(folder).length, 0);
}
const resultPath = single ? path.join(root, 'scenario-' + scenarioName + '.json') :
  gallerySnapshot ? galleryResult : path.join(root, 'result.json');
const evidence = fs.existsSync(resultPath) ? JSON.parse(fs.readFileSync(resultPath, 'utf8')) :
  { stage: 'starting', root, started: new Date().toISOString() };
let continuing;
let previousReceiptId = 0;
if (resuming && !single) {
  const previous = new DatabaseSync(path.join(archive, '.time-cut', 'imports.sqlite3'), { readOnly: true, timeout: 3000 });
  try {
    const baselinePath = path.join(root, 'result.json');
    const legacyBaseline = gallerySnapshot && fs.existsSync(baselinePath) &&
      JSON.parse(fs.readFileSync(baselinePath, 'utf8')).legacyBaseline === true;
    const hasIntake = previous.prepare("SELECT 1 FROM sqlite_master WHERE name='phone_runs'").get()
      && previous.prepare('SELECT 1 FROM phone_runs LIMIT 1').get();
    if (gallerySnapshot && hasIntake) {
      previousReceiptId = previous.prepare('SELECT COALESCE(MAX(id),0) AS id FROM phone_inbox').get().id;
    }
    assert.ok(hasIntake || previous.prepare('SELECT 1 FROM files LIMIT 1').get(),
      'Resume requires a saved phone intake or an existing archive');
    if (hasIntake) {
      const db = new DatabaseSync(path.join(profile, 'import-control.sqlite3'), { readOnly: true, timeout: 3000 });
      try {
        const selected = JSON.parse(db.prepare('SELECT data FROM config WHERE id=1').get().data);
        if (legacyBaseline) assert.equal(path.resolve(selected.root), archive, 'The legacy profile points at another archive');
        if ((selected.auto || legacyBaseline) && path.resolve(selected.root) === archive) {
          continuing = {
            historyId: db.prepare('SELECT COALESCE(MAX(id),0) AS n FROM history').get().n,
            safeAt: previous.prepare('SELECT COALESCE(MAX(safe_at),0) AS n FROM phone_runs').get().n,
          };
        }
      } finally { db.close(); }
    }
  } finally {
    previous.close();
  }
  delete evidence.error;
  delete evidence.finished;
  evidence.started = new Date().toISOString();
}
const photoTypes = new Set(['.jpg', '.jpeg', '.heic', '.heif', '.hif', '.png', '.gif', '.webp', '.dng', '.tif', '.tiff', '.bmp', '.avif', '.jxl']);
const videoTypes = new Set(['.mp4', '.mov', '.m4v', '.3gp', '.3g2', '.avi', '.mkv', '.mts', '.webm', '.wmv', '.mpg', '.mpeg', '.ts']);
if (!resuming) fs.mkdirSync(archive, { recursive: true });
function save(fields) {
  Object.assign(evidence, fields);
  const draft = resultPath.replace(/\.json$/, '.next.json');
  fs.writeFileSync(draft, JSON.stringify(evidence, null, 2));
  try {
    fs.renameSync(draft, resultPath);
  } catch (error) {
    // Another Windows process may hold the previous report open momentarily.
    if (single || !['EACCES', 'EPERM', 'EBUSY'].includes(error.code)) throw error;
  }
}
const compact = ({ phase, scanned, discovered, added, skipped, failed, inventory, album_scope, safe_at, capture_started_at }) =>
  ({ phase, scanned, discovered, added, skipped, failed, ...(album_scope ? { album_scope } : {}), ...(inventory ? { inventory } : {}),
    ...(safe_at ? { safe_at, capture_started_at, cableMs: safe_at - capture_started_at } : {}) });
const matchesDevice = name => android ? name === deviceName : /iPhone/i.test(name);
const fromPhone = report => !path.isAbsolute(report.source) && matchesDevice(report.source);
if (resuming && !single && !gallerySnapshot && !evidence.firstTransfer) {
  const history = new DatabaseSync(path.join(profile, 'import-control.sqlite3'), { readOnly: true, timeout: 3000 });
  try {
    const first = history.prepare('SELECT data FROM history ORDER BY id').all()
      .map(row => JSON.parse(row.data)).find(report => fromPhone(report) && report.added > 0);
    if (first) evidence.firstTransfer = compact(first);
  } finally {
    history.close();
  }
}
const singleModule = single ? await import('./test_timecut_android_changes.mjs') : null;
if (single) {
  const context = { executable, root, archive, profile, deviceName, scenarioName, evidence, save, gallerySnapshot };
  if (!singleModule.prepare(context)) process.exit(0);
}
save(android ? { device: deviceName } : {});
if (gallerySnapshot) {
  if (evidence.shellSha256) assert.equal(evidence.shellSha256, gallerySnapshot.inventorySha256, 'Scenario baseline changed');
  save({ expectedScope: galleryScope, shellEntries: gallerySnapshot.entries, shellSha256: gallerySnapshot.inventorySha256 });
}
console.log(root);

const port = await new Promise((resolve, reject) => {
  const server = net.createServer().once('error', reject).listen(0, '127.0.0.1', () => {
    const { port } = server.address();
    server.close(() => resolve(port));
  });
});
const app = spawn(executable, [], {
  env: {
    ...process.env,
    TIMECUT_DATA_DIR: profile,
    WEBVIEW2_USER_DATA_FOLDER: path.join(root, single ? 'webview-' + scenarioName : 'webview'),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-debugging-address=127.0.0.1`,
  },
  windowsHide: true,
  stdio: 'ignore',
});
let socket, ingest, catalog, history;
if (single) save({ appPid: app.pid });
let nextId = 0;
const pending = new Map();
const errors = [];

async function waitFor(check, label, seconds = Infinity, interval = 500) {
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    assert.equal(app.exitCode, null, 'Time-cut exited while ' + label);
    try {
      const value = await check();
      if (value) return value;
    } catch (error) {
      if (!(error.code === 'ERR_SQLITE_ERROR' && [5, 6, 517].includes(error.errcode))) throw error;
    }
    await delay(interval);
  }
  throw Error('Timed out while ' + label);
}

function send(method, params) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(Error('WebView did not respond'));
    }, 30000);
    pending.set(id, { resolve, reject, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
}

async function page(fn, value = null) {
  const result = await send('Runtime.evaluate', {
    expression: `(${fn.toString()})(${JSON.stringify(value)})`, awaitPromise: true, returnByValue: true,
  });
  if (result.exceptionDetails) throw Error('Time-cut rejected a test operation');
  return result.result.value;
}
const invoke = (command, args = {}) => page(({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args });
const usb = async () => {
  const state = await invoke('timecut_status');
  assert.ok(state.devices.length <= 1 && state.devices.every(matchesDevice),
    'USB acceptance expects just the selected phone');
  if (android && evidence.lastDeviceCount !== state.devices.length) {
    save({lastDeviceCount:state.devices.length, deviceTransitions:[...(evidence.deviceTransitions || []),
      {time:new Date().toISOString(), count:state.devices.length, appPid:app.pid}]});
  }
  return state;
};
const latestHistoryId = () => history.prepare('SELECT COALESCE(MAX(id),0) AS id FROM history').get().id;
function completedPhoneRunAfter(id, afterSafeAt) {
  return history.prepare('SELECT id,data FROM history WHERE id > ? ORDER BY id DESC').all(id)
    .map(row => ({ id: row.id, ...JSON.parse(row.data) }))
    .find(report => fromPhone(report) && report.safe_at > afterSafeAt && !report.local_errors
      && report.failed === 0 && report.added + report.skipped === report.discovered && report.discovered > 0);
}

async function runAcceptance() {
  const tab = await waitFor(async () => {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(1000) });
      return (await response.json()).find(tab => tab.type === 'page' && /tauri|localhost/i.test(tab.url));
    } catch {
      return null;
    }
  }, 'opening the packaged app', 45);
  socket = new WebSocket(tab.webSocketDebuggerUrl);
  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(data);
    const request = pending.get(message.id);
    if (request) {
      clearTimeout(request.timer);
      pending.delete(message.id);
      if (message.error) request.reject(Error(message.error.message));
      else request.resolve(message.result);
    }
    if (message.method === 'Runtime.exceptionThrown') errors.push(message.method);
  });
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  await send('Runtime.enable', {});
  await waitFor(() => page(() => !!document.querySelector('[data-testid=import-open]')), 'loading the import screen', 30);
  const version = await invoke('plugin:app|version');
  if (single) {
    await singleModule.measure({ archive, profile, evidence, save, version, invoke, usb, waitFor, gallerySnapshot });
    return;
  }
  if (!continuing) await invoke('timecut_configure', { path: archive, auto: false });
  const owner = await waitFor(async () => {
    const { target } = await invoke('timecut_status');
    return target && path.resolve(target.root) === archive && target;
  }, 'opening the selected archive', 30);
  if (owner.auto && !continuing) await invoke('timecut_set_auto', { auto: false });
  await page(() => {
    if (!document.querySelector('[data-testid=import-dialog]')) document.querySelector('[data-testid=import-open]').click();
  });
  await waitFor(async () => (await usb()).devices.length === 1, 'detecting the selected phone', 30);
  history = new DatabaseSync(path.join(profile, 'import-control.sqlite3'), { readOnly: true, timeout: 3000 });
  const archiveDb = path.join(archive, '.time-cut', 'imports.sqlite3');
  ingest = new DatabaseSync(archiveDb, { readOnly: true, timeout: 3000 });
  const stats = ingest.prepare('SELECT COUNT(*) AS originals, COALESCE(SUM(size),0) AS bytes FROM files');
  const firstPhoto = ingest.prepare(`SELECT size FROM files WHERE ${[...photoTypes].map(ext => `lower(substr(path,-${ext.length}))=?`).join(' OR ')} LIMIT 1`);
  let firstInboxPhoto;
  function newPhotoReceived() {
    if (!firstInboxPhoto && ingest.prepare("SELECT 1 FROM sqlite_master WHERE name='phone_inbox'").get()) {
      firstInboxPhoto = ingest.prepare(`SELECT 1 FROM phone_inbox WHERE blob IS NOT NULL AND id>? AND
        (${[...photoTypes].map(ext => `lower(substr(entry,-${ext.length}))=?`).join(' OR ')}) LIMIT 1`);
    }
    return firstInboxPhoto ? firstInboxPhoto.get(previousReceiptId, ...photoTypes) : !resuming && firstPhoto.get(...photoTypes);
  }
  const started = Date.now();
  let lastActivity = Date.now();
  let lastDiskCheck = 0;
  let previousProgress;
  const expectedMinimum = gallerySnapshot?.entries ?? evidence.firstTransfer?.discovered ?? 1;
  const transferAfter = continuing?.historyId ?? latestHistoryId();
  save({ stage: 'transferring',
    version: evidence.version ?? version, verificationVersion: version, owner, appPid: app.pid });
  if (!continuing || !owner.auto) await invoke('timecut_set_auto', { auto: true });
  const result = await waitFor(async () => {
    const state = await usb();
    const report = compact(state.report);
    const files = stats.get();
    const progress = observePhoneProgress(previousProgress, state.report, files.originals, gallerySnapshot && galleryScope);
    previousProgress = progress.previous;
    if (progress.advanced) lastActivity = Date.now();
    const elapsedMs = Date.now() - started;
    if (elapsedMs - lastDiskCheck > 30000) {
      lastDiskCheck = elapsedMs;
      const disk = fs.statfsSync(archive);
      if (disk.bavail * disk.bsize < 2 * 1024 ** 3) throw Error('The test volume has less than 2 GiB of free space');
    }
    if (!evidence.firstPhotoMs && newPhotoReceived()) {
      save({ firstPhotoMs: elapsedMs, firstPhotoWhileScanning: state.busy });
    }
    if (!evidence.firstVisibleMs && Number((await invoke('get_album_visible_counts'))[owner.album]) > 0) {
      save({ firstVisibleMs: elapsedMs });
    }
    if (!evidence.byteProgress) {
      const byteProgress = await page(() => {
        const bar = document.querySelector('[data-testid=import-current-file] progress');
        return bar && bar.value > 0 && bar.value < bar.max ? { bytes: bar.value, total: bar.max } : null;
      });
      if (byteProgress) save({ byteProgress });
    }
    const issues = state.report.issues.flatMap(issue => issue.match(/0x[0-9a-f]{8}|手机原件流提前结束|手机原件长度在传输途中发生变化|未看到手机存储|暂不可读|拒绝访问/gi) ?? []);
    const received = gallerySnapshot
      ? { lifetimeReceipts: state.report.total_received, lifetimeReceiptBytes: state.report.total_received_bytes,
          attemptReceivedBytes: state.report.received_bytes }
      : { receipts: state.report.total_received, receiptBytes: state.report.total_received_bytes };
    save({ elapsedMs, report, ...received, compared: state.report.compared_bytes, rechecked: state.report.rechecked,
      checkedFolders: state.report.checked_folders, listedItems: state.report.listed_items, listedFolders: state.report.listed_folders,
      ...files, detected: state.devices.length === 1, issueKinds: [...new Set(issues)] });
    const terminal = completedPhoneRunAfter(transferAfter, continuing?.safeAt ?? started - 1);
    if (terminal) {
      if (gallerySnapshot) {
        assert.equal(terminal.album_scope, galleryScope);
        assert.equal(terminal.rechecked, gallerySnapshot.entries);
        const rows = ingest.prepare('SELECT id FROM phone_runs WHERE safe_at=? AND inventory=?').all(terminal.safe_at, terminal.inventory);
        assert.equal(rows.length, 1);
        const comparison = compareGalleryRun(ingest, rows[0].id, gallerySnapshot);
        save({ independentTransferInventory: comparison });
      }
      assert.ok(terminal.discovered >= expectedMinimum,
        `This scan exposed only ${terminal.discovered} items; the previous scan found ${expectedMinimum}`);
      save({ transferHistoryId: terminal.id });
      return compact(terminal);
    }
    if (Date.now() - lastActivity > 12 * 60 * 1000) throw Error('The device made no transfer progress for 12 minutes');
    return null;
  }, 'copying all media exposed over USB', Infinity, 800);
  assert.ok(result, 'An initial USB report is required');
  save({ stage: 'indexing', transfer: result });
  if (result.safe_at) {
    await waitFor(async () => {
      const state = await usb();
      if (state.report.local_errors) throw Error(state.report.local_error || 'Some originals could not be filed');
      if (gallerySnapshot) {
        const spool = galleryInbox(ingest, evidence.independentTransferInventory);
        if (spool.released || spool.outstanding) return false;
        save({ retainedUnreleasedAfterTransfer: spool.unreleased });
        return true;
      }
      return ingest.prepare('SELECT count(*) AS count FROM phone_inbox WHERE released=1').get().count === 0;
    },
      'filing originals locally after the phone is released', Infinity, 2000);
  }
  catalog = new DatabaseSync(path.join(profile, 'libraries', owner.library + '.db'), { readOnly: true, timeout: 3000 });
  const files = ingest.prepare('SELECT path,size,digest FROM files').all();
  let scopedFiles = files;
  if (gallerySnapshot) {
    const runId = evidence.independentTransferInventory.runId;
    const keys = new Set(ingest.prepare('SELECT DISTINCT digest,size FROM phone_manifest WHERE run_id=?')
      .all(runId).map(row => row.digest + '/' + row.size));
    scopedFiles = files.filter(row => keys.has(row.digest + '/' + row.size));
    assert.equal(new Set(scopedFiles.map(row => row.digest + '/' + row.size)).size, keys.size,
      'A current album digest or size is missing from the archive');
    save({ scopedReceiptRows: gallerySnapshot.entries, scopedUniqueContents: keys.size, scopedArchivePaths: scopedFiles.length });
  }
  if (result.safe_at) {
    const normalize = name => path.normalize(name).toLowerCase();
    const originals = files.filter(file => !['.aae', '.xmp'].includes(path.extname(file.path).toLowerCase()));
    const indexed = catalog.prepare('SELECT b.path AS directory,a.name FROM afiles a JOIN afolders b ON a.folder_id=b.id WHERE b.album_id=?');
    await waitFor(() => {
      const names = new Set(indexed.all(owner.album).map(row => normalize(path.join(row.directory, row.name))));
      return originals.every(file => names.has(normalize(path.join(archive, file.path))));
    }, 'checking every archived photo and video appears in the catalog', 1800, 3000);
  } else {
    await waitFor(async () => !(await invoke('timecut_indexes')).some(job =>
      job.library === owner.library && job.album === owner.album), 'finishing the catalog index', 1800, 1500);
  }
  const media = catalog.prepare(`SELECT count(*) AS indexedMedia,
    count(*) FILTER (WHERE a.file_type=2 AND NOT EXISTS(SELECT 1 FROM afiles f WHERE f.live_photo_video_id=a.id)) AS videos,
    count(*) FILTER (WHERE a.file_type IN (1,3)) AS photos,
    count(*) FILTER (WHERE a.live_photo_video_id IS NOT NULL) AS livePhotos,
    count(*) FILTER (WHERE a.taken_date IS NOT NULL) AS dated,
    count(*) FILTER (WHERE a.geo_name IS NOT NULL) AS located
    FROM afiles a JOIN afolders b ON a.folder_id=b.id WHERE b.album_id=?`).get(owner.album);
  assert.ok(media.indexedMedia > 0 && media.photos > 0);
  assert.equal(result.added + result.skipped, result.discovered, 'Every visible entry was accounted for');
  const sources = ingest.prepare('SELECT count(*) AS count FROM sources').get().count;
  // WPD can expose multiple objects under the same display path. A source
  // entry can represent more than one archived file over the course of a scan.
  assert.ok(sources > 0 && (gallerySnapshot ? scopedFiles.length > 0 : files.length >= result.added),
    'The archive covers the completed transfers');
  const verified = verifyOriginals(executable, archive);
  assert.equal(verified.filesVerified, files.length);
  assert.equal(fs.readdirSync(path.join(archive, '.time-cut', 'staging')).length, 0);
  save({ stage: 'verifying', sources, catalog: media, originalContentVerified: verified });

  const repeatAfter = latestHistoryId();
  await invoke('timecut_retry');
  const repeat = await waitFor(() => completedPhoneRunAfter(repeatAfter, result.safe_at),
    'checking deduplication from the same connected phone', Infinity, 2500);
  assert.deepEqual([repeat.added, repeat.skipped, repeat.failed], [0, result.discovered, 0]);
  if (result.inventory) assert.equal(repeat.inventory, result.inventory);
  save({ repeat: compact(repeat), repeatHistoryId: repeat.id });
  if (android) {
    const { verifyAndroidFiling } = await import('./verify_timecut_android.mjs');
    const filing = verifyAndroidFiling(executable, archive, profile, owner.album, owner.library, repeat, gallerySnapshot);
    save({ androidFiling: filing });
  }
  const repeatSource = fs.mkdtempSync(path.join(root, 'duplicate-sample-'));
  const sample = scopedFiles.find(file => photoTypes.has(path.extname(file.path).toLowerCase()));
  assert.ok(sample);
  fs.copyFileSync(path.resolve(archive, sample.path), path.join(repeatSource, 'sample' + path.extname(sample.path)));
  await invoke('timecut_folder', { path: repeatSource, indexOnly: false });
  const crossSource = await waitFor(async () => (await invoke('timecut_status')).recent.find(item => item.source === repeatSource),
    'recognizing the same bytes from a local folder', 120, 1000);
  assert.deepEqual([crossSource.added, crossSource.skipped, crossSource.failed], [0, 1, 0]);
  save({ crossSource: compact(crossSource) });
  let organizingProgress = '';
  let lastOrganizingProgress = Date.now();
  const ready = await waitFor(async () => {
    const scenes = await invoke('timecut_scene_status', { library: owner.library });
    const faces = await invoke('get_face_stats');
    save({ scenes, faces, people: catalog.prepare('SELECT count(DISTINCT person_id) AS count FROM faces').get().count });
    if (scenes.error) throw Error('Automatic classification failed');
    const snapshot = JSON.stringify([scenes.counts, faces.processed, faces.faces, evidence.people]);
    if (snapshot !== organizingProgress) {
      organizingProgress = snapshot;
      lastOrganizingProgress = Date.now();
    }
    if (Date.now() - lastOrganizingProgress > 12 * 60 * 1000) throw Error('Automatic organization made no progress for 12 minutes');
    return !scenes.counts.pending && faces.unprocessed === 0 && !(await invoke('is_face_indexing'))[0];
  }, 'organizing imported media on-device', 10800, 3000);
  assert.ok(ready);
  const scopedPaths = new Set(scopedFiles.map(file => path.resolve(archive, file.path).toLowerCase()));
  const independent = catalog.prepare(`SELECT b.path AS directory, a.name
    FROM afiles a JOIN afolders b ON b.id=a.folder_id
    WHERE b.album_id=? AND a.file_type=2 AND a.size>10000
      AND NOT EXISTS(SELECT 1 FROM afiles f WHERE f.live_photo_video_id=a.id)
    ORDER BY a.size`).all(owner.album).find(row => scopedPaths.has(path.resolve(row.directory, row.name).toLowerCase()));
  if (independent) {
    const directory = path.join(path.dirname(executable), 'ffmpeg');
    const video = path.join(independent.directory, independent.name);
    assert.ok(videoTypes.has(path.extname(video).toLowerCase()));
    const decoded = spawnSync(path.join(directory, 'ffmpeg-x86_64-pc-windows-msvc.exe'),
      ['-xerror', '-nostdin', '-v', 'error', '-i', video, '-map', '0:v:0', '-map', '0:a?', '-f', 'null', '-'],
      { windowsHide: true, timeout: 180000 });
    assert.equal(decoded.status, 0, 'An archived USB video decodes in full');
    save({ decodedVideo: true });
  }
  assert.deepEqual(errors, []);
  save({ stage: 'passed', finished: new Date().toISOString() });
}
try {
  await runAcceptance();
} catch (error) {
  save({ stage: 'failed', error: error.message });
  console.error(error.message);
  process.exitCode = 1;
} finally {
  if (socket?.readyState === WebSocket.OPEN) await invoke('timecut_set_auto', { auto: false }).catch(() => {});
  socket?.close();
  catalog?.close();
  ingest?.close();
  history?.close();
  if (single && app.exitCode === null && app.signalCode === null) {
    const stopped = new Promise(resolve => app.once('exit', resolve));
    app.kill();
    await Promise.race([stopped, delay(5000, null, { ref: false })]);
    assert.ok(app.exitCode !== null || app.signalCode !== null, 'The isolated application did not exit');
  } else if (app.exitCode === null) app.kill();
  if (single) singleModule.disarm(profile);
}
