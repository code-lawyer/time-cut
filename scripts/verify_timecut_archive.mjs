import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

export function verifyOriginals(executable, archive) {
  const run = spawnSync(executable, ['--verify-archive', archive], { encoding: 'utf8', windowsHide: true });
  if (run.error) throw run.error;
  assert.equal(run.status, 0, run.stderr.trim() || run.stdout.trim() || 'Archive verification failed');
  const totals = JSON.parse(run.stdout);
  assert.ok(totals.filesVerified > 0 && totals.failed === 0);
  return totals;
}

// Inspect only the selected album. A verification must not wake the import worker or change its auto setting.
export function verifyLocalGallery(executable, archive, profile) {
  const totals = verifyOriginals(executable, archive);
  const open = filename => new DatabaseSync(filename, { readOnly: true, timeout: 3000 });
  const control = open(path.join(profile, 'import-control.sqlite3'));
  let ingest, catalog;
  try {
    const owner = JSON.parse(control.prepare('SELECT data FROM config WHERE id=1').get().data);
    const normalize = name => path.toNamespacedPath(path.resolve(name)).toLowerCase();
    assert.equal(normalize(owner.root), normalize(archive));
    assert.ok(/^[\w-]+$/.test(owner.library), 'Invalid catalog identifier');
    ingest = open(path.join(archive, '.time-cut', 'imports.sqlite3'));
    catalog = open(path.join(profile, 'libraries', owner.library + '.db'));
    const rows = catalog.prepare(`SELECT b.path AS directory, a.name, a.id, a.has_faces,
        s.category, s.manual_category FROM afiles a JOIN afolders b ON b.id=a.folder_id
        LEFT JOIN timecut_scenes s ON s.file_id=a.id WHERE b.album_id=?`).all(owner.album);
    const names = new Set(rows.map(row => normalize(path.join(row.directory, row.name))));
    let mediaVerified = 0;
    for (const file of ingest.prepare('SELECT path FROM files').iterate()) {
      if (/\.(aae|xmp)$/i.test(file.path)) continue;
      assert.ok(names.has(normalize(path.join(archive, file.path))), 'An archived original is missing from the catalog');
      mediaVerified++;
    }
    assert.ok(mediaVerified > 0);
    return { ...totals, mediaVerified, classifiedRows: rows.filter(row => row.manual_category || row.category).length,
      faceProcessedRows: rows.filter(row => row.has_faces > 0).length };
  } finally {
    catalog?.close();
    ingest?.close();
    control.close();
  }
}
