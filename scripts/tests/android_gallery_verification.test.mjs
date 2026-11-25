import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { compareGalleryRun, galleryInbox, galleryScope, loadGallerySnapshot, observePhoneProgress } from '../verify_timecut_android.mjs';

// Two different phone objects can share a Shell display path and size.
// These expected values come from hashing the equivalent independent Shell list.
const snapshot = {
  format: 'timecut-shell-gallery-path-size-sha256-v1', galleryRoots: ['DCIM', 'Pictures', 'Movies'],
  rootFolders: 2, entries: 3, knownBytes: 28, honorNoMedia: true,
  inventorySha256: 'a655df1108fbd4dc374228bd12c0b2c4057e56769fc74f12fa80ec2664578444',
  tokens: ['5b8a29d640d1e6d89ac469b47c1daf50acd91132e94f520d0a71a926502b6a9e',
    'c21bee99daf9db5e62f191e7701c2ba0c364edc94ef47f62efc37e6c1dd2e54e',
    'c21bee99daf9db5e62f191e7701c2ba0c364edc94ef47f62efc37e6c1dd2e54e'],
};
const photo = 'Internal\\DCIM\\shared.jpg';
const movie = 'Internal\\Pictures\\b.mov';

function database(t) {
  const db = new DatabaseSync(':memory:');
  t.after(() => db.close());
  db.exec(`
    CREATE TABLE phone_runs(id INTEGER PRIMARY KEY,source,safe_at,discovered);
    CREATE TABLE phone_lists(run_id INTEGER PRIMARY KEY,total);
    CREATE TABLE phone_run_scopes(run_id INTEGER PRIMARY KEY,scope);
    CREATE TABLE phone_list_items(run_id,ordinal,entry,version,digest,size,PRIMARY KEY(run_id,ordinal));
    CREATE TABLE phone_manifest(run_id,ordinal,entry,version,digest,size,PRIMARY KEY(run_id,ordinal));
    CREATE TABLE phone_inbox(id INTEGER PRIMARY KEY,run_id,source,entry,version,digest,size,released DEFAULT 0,
      UNIQUE(source,entry,version,digest));
    INSERT INTO phone_runs VALUES(1,'wpd:galaxy',2,3);
    INSERT INTO phone_lists VALUES(1,3);
  `);
  db.prepare('INSERT INTO phone_run_scopes VALUES(1,?)').run(galleryScope);
  const add = db.prepare('INSERT INTO phone_list_items VALUES(1,?,?,?,?,?)');
  add.run(0, photo, null, 'A', 8);
  add.run(1, photo, 'v2', 'B', 8);
  add.run(2, movie, 'v3', 'C', 12);
  db.exec('INSERT INTO phone_manifest SELECT * FROM phone_list_items');
  return db;
}

test('a saved Shell inventory includes multiplicity, order-independent checksum and known bytes', t => {
  const folder = fs.mkdtempSync(path.join(os.tmpdir(), 'timecut-gallery-test-'));
  t.after(() => fs.rmSync(folder, { recursive: true }));
  const name = path.join(folder, 'scope.json');
  fs.writeFileSync(name, JSON.stringify(snapshot));
  assert.deepEqual(loadGallerySnapshot(name), snapshot);
  for (const update of [{ tokens: [...new Set(snapshot.tokens)] }, { inventorySha256: '0'.repeat(64) },
    { format: 'timecut-shell-path-size-sha256-v1' }, { knownBytes: null }, { honorNoMedia: false }]) {
    fs.writeFileSync(name, JSON.stringify({ ...snapshot, ...update }));
    assert.throws(() => loadGallerySnapshot(name));
  }
});

test('a reused run may retain old receipts only after all current rows match Shell', t => {
  const db = database(t);
  const comparison = compareGalleryRun(db, 1, snapshot);
  assert.equal(comparison.matched, 3);
  const add = db.prepare('INSERT INTO phone_inbox(run_id,source,entry,version,digest,size,released) VALUES(?,?,?,?,?,?,?)');
  add.run(1, 'wpd:galaxy', 'Internal/Download/not-gallery.jpg', '', 'A', 8, 0);
  add.run(1, 'wpd:galaxy', photo, 'older', 'A', 8, 0);
  add.run(1, 'wpd:galaxy', photo, '', 'different-content', 8, 0);
  add.run(1, 'wpd:galaxy', movie, 'v3', 'C', 99, 0);
  add.run(2, 'wpd:other', photo, '', 'A', 8, 0);
  assert.deepEqual(galleryInbox(db, comparison), { released: 0, outstanding: 0, unreleased: 5 });
  // An exact receipt can still belong to an older run. The count must find it.
  add.run(2, 'wpd:galaxy', photo, '', 'A', 8, 0);
  assert.equal(galleryInbox(db, comparison).outstanding, 1);
  db.exec("UPDATE phone_inbox SET released=1 WHERE source='wpd:other'");
  assert.equal(galleryInbox(db, comparison).released, 1);
});

test('missing, extra, changed or unreconciled receipts cannot certify an album-only run', t => {
  const db = database(t);
  for (const sql of [
    'UPDATE phone_runs SET safe_at=NULL',
    "UPDATE phone_run_scopes SET scope='old'",
    "UPDATE phone_list_items SET entry='Internal/Android/shared.jpg' WHERE ordinal=1",
    'DELETE FROM phone_list_items WHERE ordinal=0',
    "INSERT INTO phone_list_items VALUES(1,3,'extra',NULL,'D',1)",
    'UPDATE phone_list_items SET size=NULL WHERE ordinal=0',
    'UPDATE phone_list_items SET ordinal=4 WHERE ordinal=2',
    "UPDATE phone_manifest SET digest='wrong' WHERE ordinal=0",
    "INSERT INTO phone_manifest VALUES(1,3,'extra',NULL,'D',1)",
  ]) {
    db.exec('SAVEPOINT change');
    db.exec(sql);
    assert.throws(() => compareGalleryRun(db, 1, snapshot), sql);
    db.exec('ROLLBACK TO change; RELEASE change');
  }
  assert.throws(() => compareGalleryRun(db, 1, { ...snapshot, knownBytes: 27 }));
});

test('a legacy status cannot mask progress in the smaller resumed run', () => {
  const active = { album_scope: galleryScope, source: 'phone', capture_started_at: 1, receive_attempt: 2,
    scanned: 2, added: 0, skipped: 0 };
  let state;
  function check(report, expected) {
    const result = observePhoneProgress(state, report, 0, galleryScope);
    assert.equal(result.advanced, expected);
    state = result.previous;
  }
  check({ ...active, album_scope: undefined, scanned: 20000 }, false);
  check(active, true);
  check(active, false);
  check({ ...active, checked_folders: 1 }, true);
  check({ ...active, listed_items: 8 }, true);
  check({ ...active, compared_bytes: 128 }, true);
  check({ ...active, rechecked: 1 }, true);
  check({ ...active, current: 'first', transferred_bytes: 1024 }, true);
  check({ ...active, current: 'second', transferred_bytes: 24 }, true);
  check({ ...active, current: 'second', transferred_bytes: 24 }, false);
  check({ ...active, receive_attempt: 3, scanned: 1 }, true);
});
