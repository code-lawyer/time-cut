import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { verifyLocalGallery } from './verify_timecut_archive.mjs';

const open = file => new DatabaseSync(file, { readOnly: true, timeout: 3000 });
const hasTable = (db, name) => Boolean(db.prepare(
  "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?"
).get(name));
const normalized = value => process.platform === 'win32'
  ? path.toNamespacedPath(path.resolve(value)).toLowerCase() : path.resolve(value);
const attachment = name => path.extname(name).toLowerCase().match(/^\.(aae|xmp)$/)?.[1];
export const galleryScope = 'android-albums-v2';
const sha256 = value => createHash('sha256').update(value).digest('hex');

// The first status after opening a resumed app can still show the old, wider inventory.
// Directory validation, original comparisons and final reconciliation also make progress without new copies.
export function observePhoneProgress(previous, report, originals, scope) {
  if (scope && report.album_scope !== scope) return { previous, advanced: false };
  const session = [report.source, report.capture_started_at, report.receive_attempt].join('|');
  const sameSession = previous?.session === session;
  const counters = [report.scanned, report.added + report.skipped, originals, report.known_folders,
    report.checked_folders, report.listed_folders, report.listed_items, report.rechecked,
    report.compared_bytes, report.received_bytes].map(value => Number.isFinite(value) ? value : 0);
  const advanced = counters.some((n, i) => n > (sameSession ? previous.counters[i] : 0)) ||
    (report.transferred_bytes ?? 0) > (sameSession && report.current === previous.current && report.phase === previous.phase
      ? previous.bytes : 0);
  return { previous: { session, counters: counters.map((n, i) => Math.max(n, sameSession ? previous.counters[i] : 0)),
    current: report.current, phase: report.phase, bytes: report.transferred_bytes ?? 0 }, advanced };
}

export function loadGallerySnapshot(filename) {
  const s = JSON.parse(fs.readFileSync(filename, 'utf8'));
  assert.equal(s.format, 'timecut-shell-gallery-path-size-sha256-v1');
  assert.deepEqual(s.galleryRoots, ['DCIM', 'Pictures', 'Movies']);
  assert.equal(s.honorNoMedia, true, 'The independent inventory must honor visible .nomedia markers');
  assert.ok(s.rootFolders > 0 && Number.isSafeInteger(s.entries) && s.entries > 0 &&
    Number.isSafeInteger(s.knownBytes) && s.knownBytes >= 0 &&
    Array.isArray(s.tokens) && s.tokens.length === s.entries, 'Invalid independent gallery inventory');
  assert.ok(s.tokens.every((t, i) => /^[a-f0-9]{64}$/.test(t) && (i === 0 || s.tokens[i - 1] <= t)));
  assert.equal(sha256(s.tokens.join('')), s.inventorySha256);
  return s;
}

// Compare *every* app entry (including extras) against the independent Shell inventory.
// The manifest join additionally proves that every such listing has a reconciled receipt.
export function compareGalleryRun(db, runId, snapshot) {
  const run = db.prepare(`SELECT r.safe_at,r.discovered,l.total,s.scope FROM phone_runs r
    JOIN phone_lists l ON l.run_id=r.id JOIN phone_run_scopes s ON s.run_id=r.id WHERE r.id=?`).get(runId);
  assert.ok(run?.safe_at && run.scope === galleryScope, 'A completed Android album run is required');
  assert.equal(run.discovered, snapshot.entries);
  assert.equal(run.total, snapshot.entries);
  const tokens = [];
  let knownBytes = 0;
  for (const item of db.prepare('SELECT ordinal,entry,size FROM phone_list_items WHERE run_id=? ORDER BY ordinal').iterate(runId)) {
    assert.equal(item.ordinal, tokens.length, 'Android inventory ordinals are incomplete');
    assert.ok(typeof item.entry === 'string' && Number.isSafeInteger(item.size) && item.size >= 0);
    tokens.push(sha256(item.entry.replaceAll('\\', '/') + '\0' + item.size));
    knownBytes += item.size;
  }
  tokens.sort();
  assert.equal(tokens.length, snapshot.entries);
  assert.equal(sha256(tokens.join('')), snapshot.inventorySha256, 'Android inventory differs from Shell');
  assert.equal(knownBytes, snapshot.knownBytes);
  const matched = db.prepare(`SELECT COUNT(*) AS n FROM phone_list_items l JOIN phone_manifest m
    ON m.run_id=l.run_id AND m.ordinal=l.ordinal AND m.entry=l.entry AND m.version IS l.version
    AND m.size=l.size AND m.digest=l.digest WHERE l.run_id=?`).get(runId).n;
  assert.equal(matched, snapshot.entries);
  assert.equal(db.prepare('SELECT COUNT(*) AS n FROM phone_manifest WHERE run_id=?').get(runId).n, matched);
  return { runId, scope: run.scope, matched, knownBytes, inventorySha256: snapshot.inventorySha256 };
}

// A pre-migration spool can contain an earlier version or an excluded path from the
// same run. Only a fully matched current manifest authorizes this distinction.
export function galleryInbox(db, comparison) {
  assert.equal(comparison.scope, galleryScope);
  assert.ok(comparison.matched > 0);
  const released = db.prepare('SELECT COUNT(*) AS n FROM phone_inbox WHERE released=1').get().n;
  const outstanding = db.prepare(`SELECT COUNT(DISTINCT i.id) AS n FROM phone_runs r
    JOIN phone_manifest m ON m.run_id=r.id JOIN phone_inbox i ON i.source=r.source AND i.entry=m.entry
      AND i.version=COALESCE(m.version,'') AND i.digest=m.digest AND i.size=m.size WHERE r.id=?`).get(comparison.runId).n;
  const unreleased = db.prepare('SELECT COUNT(*) AS n FROM phone_inbox WHERE released=0').get().n;
  return { released, outstanding, unreleased };
}

// Verify only local evidence. The caller supplies the final phone report and the destination
// it selected in the packaged app. Nothing returned contains device IDs, paths or filenames.
export function verifyAndroidFiling(exe, archive, profile, albumId, library, report, gallerySnapshot = null) {
  assert.ok(Number.isSafeInteger(albumId) && /^[\w-]+$/.test(library), 'Invalid destination');
  assert.ok(report?.safe_at > 0 && report.discovered > 0 && report.failed === 0
    && report.added + report.skipped === report.discovered, 'Phone report is incomplete');

  const control = open(path.join(profile, 'import-control.sqlite3'));
  let ingest, catalog;
  try {
    const owner = JSON.parse(control.prepare('SELECT data FROM config WHERE id=1').get().data);
    assert.ok(owner.library === library && owner.album === albumId && normalized(owner.root) === normalized(archive),
      'The selected archive and catalog do not match the import configuration');
    for (const table of ['indexing', 'importing', 'index_file_retries']) {
      if (table === 'index_file_retries' && !hasTable(control, table)) continue;
      assert.ok(!control.prepare(`SELECT 1 FROM ${table} WHERE library=? AND album=? LIMIT 1`).get(library, albumId),
        'The selected album still has unfinished import or indexing work');
    }
    catalog = open(path.join(profile, 'libraries', library + '.db'));
    const album = catalog.prepare('SELECT path FROM albums WHERE id=?').get(albumId);
    assert.ok(album && normalized(album.path) === normalized(archive), 'Gallery is mapped to a different archive');

    const originals = verifyLocalGallery(exe, archive, profile);
    ingest = open(path.join(archive, '.time-cut', 'imports.sqlite3'));
    const run = ingest.prepare(`SELECT id,source,safe_at,discovered,inventory FROM phone_runs
      WHERE safe_at IS NOT NULL ORDER BY safe_at DESC,id DESC LIMIT 1`).get();
    assert.ok(run && run.source.startsWith('wpd:') && run.safe_at === report.safe_at
      && run.discovered === report.discovered && Boolean(run.inventory) && run.inventory === report.inventory,
      'The latest completed USB manifest differs from the phone report');
    if (report.device_key) {
      const saved = control.prepare("SELECT data FROM history WHERE json_extract(data,'$.safe_at')=? ORDER BY id DESC LIMIT 1")
        .get(run.safe_at);
      assert.ok(saved && JSON.parse(saved.data).device_key === report.device_key, 'Phone identity differs from saved history');
    }
    let comparison, spool;
    if (gallerySnapshot) {
      assert.equal(report.album_scope, galleryScope);
      comparison = compareGalleryRun(ingest, run.id, gallerySnapshot);
      spool = galleryInbox(ingest, comparison);
      assert.ok(spool.released === 0 && spool.outstanding === 0, 'Current gallery receipts remain unfiled');
    } else {
      assert.ok(!ingest.prepare('SELECT 1 FROM phone_inbox WHERE run_id=? LIMIT 1').get(run.id),
        'The completed USB run still has unfiled receipts');
    }

    const byDigest = new Map();
    const filedPaths = new Map();
    for (const file of ingest.prepare('SELECT digest,size,path FROM files').iterate()) {
      const key = file.digest + '/' + file.size;
      if (!byDigest.has(key)) byDigest.set(key, []);
      byDigest.get(key).push(file.path);
      filedPaths.set(normalized(path.join(archive, file.path)), file.digest);
    }
    assert.equal(filedPaths.size, originals.filesVerified, 'Archive file count differs from the native hash check');

    const listAvailable = hasTable(ingest, 'phone_list_items');
    if (listAvailable) {
      const count = ingest.prepare('SELECT count(*) AS n FROM phone_list_items WHERE run_id=?').get(run.id).n;
      const matched = ingest.prepare(`SELECT count(*) AS n FROM phone_list_items l JOIN phone_manifest m
        ON m.run_id=l.run_id AND m.ordinal=l.ordinal WHERE m.run_id=? AND l.entry=m.entry AND l.version IS m.version
        AND l.digest=m.digest AND (l.size IS NULL OR l.size=m.size)`).get(run.id).n;
      assert.ok(count === run.discovered && matched === count, 'USB inventory and per-item receipts differ');
    }
    const selected = new Set();
    const matchedDigests = new Set();
    const verifiedKinds = new Set();
    const attachments = { aae: 0, xmp: 0 };
    let receiptsVerified = 0;
    for (const receipt of ingest.prepare(`SELECT ordinal,entry,digest,size FROM phone_manifest
        WHERE run_id=? ORDER BY ordinal`).iterate(run.id)) {
      assert.equal(receipt.ordinal, receiptsVerified, 'USB manifest ordinals are incomplete');
      const key = receipt.digest + '/' + receipt.size;
      const sidecar = attachment(receipt.entry);
      if (sidecar) attachments[sidecar]++;
      const kindKey = key + '/' + (sidecar ?? 'media');
      if (!verifiedKinds.has(kindKey)) {
        const matching = (byDigest.get(key) ?? []).filter(file => attachment(file) === sidecar);
        assert.ok(matching.length, 'A USB receipt has no matching archived media or attachment of the same size and digest');
        for (const file of matching) selected.add(normalized(path.join(archive, file)));
        verifiedKinds.add(kindKey);
      }
      matchedDigests.add(key);
      receiptsVerified++;
    }
    assert.equal(receiptsVerified, run.discovered, 'USB receipt count differs from the completed inventory');
    if (hasTable(ingest, 'sidecar_paths')) {
      for (const row of ingest.prepare('SELECT owner,digest,path FROM sidecar_paths').iterate()) {
        const file = normalized(path.join(archive, row.path));
        const owner = normalized(path.join(archive, row.owner));
        if (!selected.has(file) && !selected.has(owner)) continue;
        assert.ok(attachment(row.owner) && attachment(row.owner) === attachment(row.path)
          && path.dirname(owner) === path.dirname(file)
          && filedPaths.get(file) === row.digest, 'An attachment disagrees with its owner record');
      }
    }

    const catalogRows = catalog.prepare(`SELECT a.id,a.name,b.path,a.file_type,a.media_subtype,a.has_faces,
      a.taken_date,a.geo_name,a.gps_latitude,a.gps_longitude,COALESCE(s.manual_category,s.category) AS category
      FROM afiles a JOIN afolders b ON a.folder_id=b.id LEFT JOIN timecut_scenes s ON s.file_id=a.id
      WHERE b.album_id=?`).all(albumId);
    const indexed = new Set();
    const categories = { travel: 0, work: 0, saved: 0, daily: 0, review: 0, unclassified: 0 };
    const media = { photos: 0, videos: 0, raw: 0, livePhotos: 0, rawJpegPairs: 0, dated: 0,
      withGps: 0, withPlace: 0, withFaces: 0, faceProcessed: 0 };
    const selectedIds = new Set();
    for (const row of catalogRows) {
      const file = normalized(path.join(row.path, row.name));
      if (!selected.has(file) || attachment(row.name)) continue;
      assert.ok(!indexed.has(file), 'Multiple catalog rows refer to one archived original');
      indexed.add(file);
      selectedIds.add(row.id);
      const kind = { 1: 'photos', 2: 'videos', 3: 'raw' }[row.file_type];
      assert.ok(kind, 'Unknown catalog media type');
      media[kind]++;
      if (row.media_subtype === 'live_photo') media.livePhotos++;
      if (row.media_subtype === 'raw_jpeg_pair') media.rawJpegPairs++;
      if (row.taken_date > 0) media.dated++;
      if (row.gps_latitude != null && row.gps_longitude != null) media.withGps++;
      if (row.geo_name) media.withPlace++;
      if (row.has_faces === 1) media.withFaces++;
      if (row.has_faces > 0) media.faceProcessed++;
      if (row.file_type !== 2) categories[Object.hasOwn(categories, row.category) ? row.category : 'unclassified']++;
    }
    const sidecarPaths = [...selected].filter(attachment).length;
    assert.equal(indexed.size + sidecarPaths, selected.size, 'A USB media path is missing from the selected album');
    const people = new Set();
    for (const row of catalog.prepare(`SELECT DISTINCT f.file_id,f.person_id FROM faces f
      JOIN afiles a ON a.id=f.file_id JOIN afolders b ON b.id=a.folder_id
      WHERE b.album_id=? AND f.person_id IS NOT NULL`).iterate(albumId)) {
      if (selectedIds.has(row.file_id)) people.add(row.person_id);
    }
    return { ...originals, ...(comparison && { independentInventory: comparison, retainedUnreleased: spool.unreleased }),
      receiptsVerified, matchedDigests: matchedDigests.size, inventoryRowsCompared: listAvailable,
      digestGroupsWithMultiplePaths: [...matchedDigests].filter(key => byDigest.get(key).length > 1).length,
      receiptMediaPathsIndexed: indexed.size, receiptAttachmentPathsVerified: sidecarPaths,
      attachments, media, people: people.size, categories };
  } finally {
    catalog?.close();
    ingest?.close();
    control.close();
  }
}
