import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';
import { getAlbumQueueIndex } from '../src/common/scanStatus.ts';

const source = await readFile(new URL('../src/composables/useTimecutIndex.js', import.meta.url), 'utf8');

async function mount(index) {
  const library = { _initialized: true, _libraryId: 'default', index };
  const job = { library: 'default', album: 1, revision: 7 };
  let requests = [job];
  let mounted, poll;
  let now = 100_000;
  const events = new Map(), acknowledged = [];
  const context = vm.createContext({ setInterval: (fn) => { poll = fn; }, clearInterval: () => {},
    Date: { now: () => now } });
  const dependencies = {
    vue: { onMounted: (fn) => { mounted = fn; }, onUnmounted: () => {}, watch: () => {} },
    '@tauri-apps/api/core': { invoke: async (name, args) => {
      if (name === 'timecut_indexes') return requests;
      assert.equal(name, 'timecut_indexed');
      acknowledged.push(args.revision);
      requests = requests.filter(r => r.revision !== args.revision);
    } },
    '@tauri-apps/api/event': { emit: async () => {}, listen: async (name, fn) => {
      events.set(name, fn);
      return () => events.delete(name);
    } },
    '@/common/api': { getAllAlbums: async () => [] },
    '@/common/scanStatus': { getAlbumQueueIndex },
    '@/stores/libraryStore': { useLibraryStore: () => library },
  };
  const module = new vm.SourceTextModule(source, { context });
  await module.link(key => new vm.SyntheticModule(Object.keys(dependencies[key]), function () {
    for (const [name, value] of Object.entries(dependencies[key])) this.setExport(name, value);
  }, { context }));
  await module.evaluate();
  module.namespace.useTimecutIndex(error => { if (error) throw error; });
  await mounted();
  return { index, job, acknowledged, poll, advance: milliseconds => { now += milliseconds; },
    async finish(complete = true, failed = 0) {
    events.get('index_finished')({payload: {library_id: 'default', album_id: 1, scan_complete: complete, failed}});
    await new Promise(resolve => setImmediate(resolve));
  } };
}

test('resumes a restored phone index and acknowledges only the revision that started', async () => {
  const run = await mount({ status: 2, albumQueue: ['1'], pausedAlbumIds: [] });
  assert.equal(run.index.status, 1);
  assert.deepEqual(run.index.albumQueue, ['1']);
  run.job.revision = 8;
  await run.poll();
  run.index.albumQueue.shift();
  run.index.status = 0;
  await run.finish();
  assert.deepEqual(run.acknowledged, [7]);
  assert.deepEqual(run.index.albumQueue, [1]);
  run.index.albumQueue.shift();
  await run.finish();
  assert.deepEqual(run.acknowledged, [7, 8]);
});

test('keeps an import revision pending when traversal finishes with a failed file', async () => {
  const run = await mount({ status: 0, albumQueue: [], pausedAlbumIds: [] });
  run.index.albumQueue.shift();
  run.index.status = 0;
  await run.finish(true, 1);
  await run.poll();
  assert.deepEqual(run.acknowledged, []);
  assert.deepEqual(run.index.albumQueue, []); // Back off before retrying the same revision.
  run.advance(30_000);
  await run.poll();
  assert.deepEqual(run.index.albumQueue, [1]);
  await run.finish();
  assert.deepEqual(run.acknowledged, [7]);
});

test('leaves an explicit pause and an unrelated paused queue head alone', async () => {
  for (const index of [
    { status: 2, albumQueue: [1], pausedAlbumIds: [1] },
    { status: 2, albumQueue: [2, 1], pausedAlbumIds: [] },
    { status: 2, albumQueue: [2], pausedAlbumIds: [2] },
  ]) {
    const initialQueue = [...index.albumQueue];
    const run = await mount(index);
    assert.equal(index.status, 2);
    assert.deepEqual(index.albumQueue, initialQueue);
    await run.finish();
    assert.deepEqual(run.acknowledged, []);
  }
});

test('does not override a recovery pause twice or claim an already running manual scan', async () => {
  const paused = await mount({ status: 2, albumQueue: [1], pausedAlbumIds: [] });
  paused.index.status = 2; // The scanner showed its per-file crash recovery prompt.
  await paused.poll();
  assert.equal(paused.index.status, 2);
  const manual = await mount({ status: 1, albumQueue: [1], pausedAlbumIds: [] });
  await manual.finish();
  assert.deepEqual(manual.acknowledged, []);
});

test('the album scanner retries the interrupted import file and retains the repeated-crash dialog', async () => {
  const content = await readFile(new URL('../src/components/Content.vue', import.meta.url), 'utf8');
  const scanner = content.slice(content.indexOf('async function processNextAlbum('), content.indexOf('// Check if current album is being indexed'));
  for (const permitted of [true, false]) {
    const indexed = [];
    const dependencies = {
      libConfig: { _libraryId: 'photos', index: { albumQueue: [7], pausedAlbumIds: [] } },
      scanProgressSession: { value: 0 }, showIndexRecoveryMsgbox: { value: false }, recoverySkipFilePath: { value: '' },
      getAlbum: async () => ({ name: 'iPhone' }), getIndexRecoveryInfo: async () => ({ album_id: 7, file_path: 'original.jpg' }),
      retryInterruptedImportIndex: async (library, album) => {
        assert.deepEqual([library, album], ['photos', 7]);
        return permitted;
      },
      indexAlbum: async (...args) => indexed.push(args),
    };
    const processAlbum = new Function(...Object.keys(dependencies), ts.transpile(scanner, { target: ts.ScriptTarget.ES2022 }) +
      ';return processNextAlbum;')(...Object.values(dependencies));
    await processAlbum();
    assert.deepEqual(indexed, permitted ? [[7, null, 'photos']] : []);
    assert.equal(dependencies.showIndexRecoveryMsgbox.value, !permitted);
  }
});
