// node scripts/test_timecut_android_changes.mjs <portable.exe> <existing android-full-* root> <Galaxy display name> <scenario>
// Re-run the identical command after stopping the interrupted test and its application.
// The scenario watermark is durable before the application starts.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { pathToFileURL } from 'node:url';

const open = (filename, readOnly = true) => new DatabaseSync(filename, { readOnly, timeout: 5000 });
const controlPath = profile => path.join(profile, 'import-control.sqlite3');
const normalize = name => path.toNamespacedPath(path.resolve(name)).toLowerCase();
const loadRun = db => db.prepare(`SELECT id,source,started,first_copy,safe_at,inventory,copies,bytes,compared,
  discovered,sessions,filed,duplicates FROM phone_runs WHERE id=?`);

export function disarm(profile) {
  const control = open(controlPath(profile), false);
  try {
    control.prepare("UPDATE config SET data=json_set(data,'$.auto',json('false')) WHERE id=1 AND json_extract(data,'$.auto')=1").run();
    assert.equal(control.prepare("SELECT json_extract(data,'$.auto') AS enabled FROM config WHERE id=1").get().enabled, 0);
  } finally { control.close(); }
}

function pidRunning(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; }
}

export function prepare({ root, archive, profile, deviceName, scenarioName, evidence, save }) {
  const baseline = JSON.parse(fs.readFileSync(path.join(root, 'result.json'), 'utf8'));
  assert.equal(baseline.device, deviceName, 'The selected device differs from the saved acceptance root');
  if (evidence.scenario) {
    assert.equal(evidence.scenario, scenarioName);
    assert.equal(evidence.device, deviceName);
    if (evidence.stage === 'passed') return false;
  }
  assert.ok(!pidRunning(evidence.appPid) && !pidRunning(baseline.appPid),
    'Stop the previous acceptance application before resuming its isolated profile');
  const db = open(path.join(archive, '.time-cut', 'imports.sqlite3'));
  const control = open(controlPath(profile));
  try {
    const owner = JSON.parse(control.prepare('SELECT data FROM config WHERE id=1').get().data);
    assert.equal(normalize(owner.root), normalize(archive));
    assert.ok(/^[\w-]+$/.test(owner.library) && Number.isInteger(owner.album));
    const sources = db.prepare('SELECT DISTINCT source FROM phone_runs').all();
    assert.ok(sources.length === 1 && sources[0].source.startsWith('wpd:'), 'Expected one archived WPD source');
    assert.equal(control.prepare('SELECT count(*) AS n FROM phone_repeat WHERE library=? AND album=?').get(owner.library, owner.album).n, 0,
      'A separate automatic verification is armed');
    if (!evidence.scenario) {
      const last = db.prepare('SELECT id,safe_at FROM phone_runs ORDER BY id DESC LIMIT 1').get();
      const existing = last.safe_at === null;
      save({ scenario: scenarioName, device: deviceName, owner: { library: owner.library, album: owner.album },
        firstAllowedRunId: existing ? last.id : last.id + 1, targetRunId: existing ? last.id : null,
        resumedOpenRun: existing, stage: 'armed', started: new Date().toISOString() });
    } else {
      assert.deepEqual(evidence.owner, { library: owner.library, album: owner.album });
    }
    const run = selectedRun(db, evidence);
    // An application that restarts with auto already on will resume this open run.
    // A closed run must be disarmed before launch so a crash after safe_at cannot start another.
    if (owner.auto && run && !run.safe_at) return true;
  } finally { control.close(); db.close(); }
  // A forcibly stopped process can leave auto enabled. Disarm its isolated profile
  // before a new worker opens it, including when the original run already reached safe_at.
  disarm(profile);
  return true;
}

export function selectedRun(db, evidence) {
  const rows = db.prepare('SELECT id FROM phone_runs WHERE id>=? ORDER BY id LIMIT 2').all(evidence.firstAllowedRunId);
  assert.ok(rows.length <= 1, 'More than one USB scan was started for this scenario');
  if (!rows.length) { assert.equal(evidence.targetRunId, null); return null; }
  if (evidence.targetRunId !== null) assert.equal(rows[0].id, evidence.targetRunId);
  return loadRun(db).get(rows[0].id);
}

export async function measure({ archive, profile, evidence, save, version, invoke, usb, waitFor }) {
  const db = open(path.join(archive, '.time-cut', 'imports.sqlite3'));
  const control = open(controlPath(profile));
  const { library, album } = evidence.owner;
  const catalog = open(path.join(profile, 'libraries', library + '.db'));
  try {
    const { target } = await invoke('timecut_status');
    assert.ok(target && normalize(target.root) === normalize(archive) && target.library === library
      && target.album === album, 'The isolated application opened a different target');
    let run = selectedRun(db, evidence);
    if (!run?.safe_at) {
      if (!target.auto) await waitFor(async () => (await usb()).devices.length === 1, 'detecting the selected Galaxy', 60);
      save({ stage: 'receiving', version, error: null });
      if (!target.auto) await invoke('timecut_set_auto', { auto: true });
      run = await waitFor(async () => {
        const current = selectedRun(db, evidence);
        if (current && evidence.targetRunId === null) save({ targetRunId: current.id, runStartedAt: current.started });
        if (current?.safe_at) return current;
        const state = await usb();
        const report = state.report;
        if (current && report.capture_started_at === current.started) {
          save({ targetRunId: current.id, runStartedAt: current.started, firstCopyAt: current.first_copy,
            attempts: current.sessions, scanned: report.scanned, discovered: report.discovered,
            added: report.added, skipped: report.skipped, failed: report.failed, pending: report.pending });
        }
        return null;
      }, 'receiving the single phone inventory', Infinity, 1000);
    }
    if (target.auto || !evidence.safeAt) await invoke('timecut_set_auto', { auto: false });
    save({ stage: 'filing', targetRunId: run.id, runStartedAt: run.started, firstCopyAt: run.first_copy,
      safeAt: run.safe_at, inventory: run.inventory, receivedFiles: run.copies, receivedBytes: run.bytes,
      comparedBytes: run.compared, attempts: run.sessions, version });
    const terminal = await waitFor(() => control.prepare(`SELECT data FROM history
      WHERE json_extract(data,'$.safe_at')=? AND json_extract(data,'$.capture_started_at')=?
        AND json_extract(data,'$.source')=? AND json_extract(data,'$.inventory')=? ORDER BY id DESC LIMIT 1`)
      .get(run.safe_at, run.started, evidence.device, run.inventory), 'reading the published phone report', 60);
    const report = JSON.parse(terminal.data);
    assert.ok(run.discovered > 0 && report.failed === 0 && report.pending === 0
      && report.rechecked === run.discovered && report.added + report.skipped === run.discovered);
    const coverage = db.prepare(`SELECT COUNT(*) AS listed,COUNT(m.digest) AS covered,COUNT(p.ordinal) AS reconciled
      FROM phone_list_items m LEFT JOIN phone_manifest p ON p.run_id=m.run_id AND p.ordinal=m.ordinal
        AND p.entry=m.entry AND p.version IS m.version AND p.digest=m.digest AND (m.size IS NULL OR p.size=m.size)
      WHERE m.run_id=?`).get(run.id);
    assert.equal(db.prepare('SELECT total FROM phone_lists WHERE run_id=?').get(run.id).total, run.discovered);
    assert.equal(db.prepare('SELECT COUNT(*) AS n FROM phone_manifest WHERE run_id=?').get(run.id).n, run.discovered);
    assert.ok(Object.values(coverage).every(n => n === run.discovered));
    await waitFor(() => {
      assert.equal(db.prepare('SELECT COUNT(*) AS n FROM phone_local_retries').get().n, 0, 'Local receipts need repair');
      return db.prepare('SELECT COUNT(*) AS n FROM phone_inbox').get().n === 0
        && !control.prepare('SELECT 1 FROM phone_work WHERE library=? AND album=?').get(library, album);
    }, 'filing all phone receipts', 3600, 1000);
    const filed = selectedRun(db, evidence);
    save({ stage: 'indexing', filedAt: evidence.filedAt ?? Date.now(), added: report.added, skipped: report.skipped,
      failed: report.failed, pending: report.pending, discovered: run.discovered, rechecked: report.rechecked,
      ...coverage, filed: filed.filed, duplicates: filed.duplicates, localPending: 0 });
    const media = db.prepare('SELECT path FROM files').all().filter(row => !/\.(?:aae|xmp)$/i.test(row.path))
      .map(row => normalize(path.join(archive, row.path)));
    assert.ok(media.length, 'No archived media to verify in the catalog');
    await waitFor(() => {
      if (control.prepare('SELECT 1 FROM indexing WHERE library=? AND album=?').get(library, album)) return false;
      const indexed = new Set(catalog.prepare(`SELECT d.path, a.name FROM afiles a JOIN afolders d
        ON d.id=a.folder_id WHERE d.album_id=?`).all(album).map(row => normalize(path.join(row.path, row.name))));
      return media.every(name => indexed.has(name));
    }, 'checking actual gallery index coverage', 1800, 1500);
    selectedRun(db, evidence);
    assert.equal(control.prepare("SELECT json_extract(data,'$.auto') AS enabled FROM config WHERE id=1").get().enabled, 0);
    save({ stage: 'passed', indexedMedia: media.length, indexedAt: Date.now(), finished: new Date().toISOString(), auto: false });
  } finally { catalog.close(); control.close(); db.close(); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const [executable, root, deviceName, scenarioName] = process.argv.slice(2);
  assert.ok(process.argv.length === 6, 'Expected executable, existing Android test root, Galaxy display name and scenario name');
  process.argv = [process.execPath, process.argv[1], executable, root, '--android-device', deviceName, '--single-scan', scenarioName];
  // Finish loading this module before the shared USB harness imports its scenario functions.
  import('./test_timecut_phone.mjs').catch(error => { console.error(error.message); process.exitCode = 1; });
}
