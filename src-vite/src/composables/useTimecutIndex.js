import { onMounted, onUnmounted, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import { getAllAlbums } from '@/common/api';
import { getAlbumQueueIndex } from '@/common/scanStatus';
import { useLibraryStore } from '@/stores/libraryStore';

// The import journal belongs to the native worker. A completed scan can only
// acknowledge the revision that was actually queued for this library and album.
export function useTimecutIndex(reportError) {
  const lib = useLibraryStore();
  const inFlight = new Map();
  const retries = new Map();
  const unsubscribe = [];
  let disposed = false;
  let subscribed = false;
  let loading = false;
  let timer;

  const keyOf = (library, album) => library + '/' + album;

  async function refreshAlbums() {
    const libraryId = lib._libraryId;
    const albums = await getAllAlbums();
    if (!disposed && albums && libraryId === lib._libraryId) {
      await emit('albums-refreshed', { libraryId, albums, includeNew: true, refreshCounts: true });
    }
  }

  async function schedule() {
    if (disposed || loading || !lib._initialized) return;
    loading = true;
    try {
      const jobs = await invoke('timecut_indexes');
      if (disposed) return;
      reportError(null);
      const live = new Set(jobs.map(job => keyOf(job.library, job.album)));
      for (const key of retries.keys()) if (!live.has(key)) retries.delete(key);
      for (const key of inFlight.keys()) if (!live.has(key)) inFlight.delete(key);
      for (const job of jobs) {
        if (job.library !== lib._libraryId) continue;
        const key = keyOf(job.library, job.album);
        if (getAlbumQueueIndex(job.album, lib.index.pausedAlbumIds) >= 0) continue;
        const retry = retries.get(key);
        if (retry?.revision === job.revision && retry.after > Date.now()) continue;
        const attempt = inFlight.get(key);
        const position = getAlbumQueueIndex(job.album, lib.index.albumQueue);
        if (position >= 0) {
          // Startup pauses the restored queue. Adopt the pending import at its head
          // once, so a later manual pause or crash-recovery dialog stays in control.
          if (position !== 0 || lib.index.status !== 2 || attempt) continue;
        } else {
          // A failed scan launch may leave the queue without emitting index_finished.
          if (attempt?.revision === job.revision && Date.now() - attempt.started < 30_000) continue;
          if (lib.index.status === 2 && lib.index.albumQueue.length) continue;
          lib.index.albumQueue.push(job.album);
        }
        inFlight.set(key, { revision: job.revision, started: Date.now() });
        lib.index.status = 1;
      }
    } catch (error) {
      reportError(error);
    } finally {
      loading = false;
    }
  }

  async function finish({ payload }) {
    const key = keyOf(payload.library_id, payload.album_id);
    const attempt = inFlight.get(key);
    if (!attempt) return;
    if (payload.scan_complete !== true) {
      inFlight.delete(key);
      const previous = retries.get(key);
      const delay = previous?.revision === attempt.revision ? Math.min(previous.delay * 2, 300_000) : 30_000;
      retries.set(key, { revision: attempt.revision, delay, after: Date.now() + delay });
      return;
    }
    // Suppress a concurrent scheduling pass until acknowledgement has committed.
    try {
      await invoke('timecut_indexed', { library: payload.library_id, album: payload.album_id, revision: attempt.revision });
      retries.delete(key);
      if (lib._libraryId === payload.library_id) {
        await refreshAlbums();
        await emit('library-total-refreshed', { source: 'timecut', libraryId: payload.library_id });
      }
    } catch (error) {
      reportError(error);
    } finally {
      if (inFlight.get(key) === attempt) inFlight.delete(key);
    }
    await schedule();
  }

  async function subscribe(event, fn) {
    const stop = await listen(event, payload => { void fn(payload).catch(reportError); });
    if (disposed) stop();
    else unsubscribe.push(stop);
  }

  watch(() => [lib._initialized, lib._libraryId], () => { if (subscribed) void schedule(); });
  onMounted(async () => {
    try {
      await Promise.all([
        subscribe('timecut-index', async () => { await schedule(); await refreshAlbums(); }),
        subscribe('index_finished', finish),
      ]);
      subscribed = true;
    } catch (error) { reportError(error); }
    if (disposed) return;
    await schedule();
    if (!disposed) timer = setInterval(schedule, 30_000);
  });
  onUnmounted(() => {
    disposed = true;
    clearInterval(timer);
    unsubscribe.forEach(stop => stop());
  });
}
