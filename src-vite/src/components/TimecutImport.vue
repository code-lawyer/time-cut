<template>
  <button ref="opener" class="import-entry" type="button" data-testid="import-open" aria-haspopup="dialog"
    :aria-expanded="opened" @mousedown.stop @click.stop="opened = true">
    <IconUpload aria-hidden="true" /><span>{{ importLabel }}</span>
  </button>

  <Teleport v-if="opened" to="body">
    <dialog ref="dialog" class="import-dialog" role="dialog" aria-modal="true" aria-labelledby="timecut-heading"
      data-testid="import-dialog" @cancel.prevent="close" @keydown.stop @keydown.tab="keepTabInDialog">
      <header class="import-header">
        <h1 id="timecut-heading" tabindex="-1">{{ t('timecut.title') }}</h1>
        <button class="close-button" type="button" :aria-label="t('timecut.close')" data-testid="import-close" @click="close">
          <IconClose aria-hidden="true" />
        </button>
      </header>

      <PhoneStatus v-if="current?.capture" :report="current" :busy="state.busy" :live-release="liveRelease"
        :connected="state.devices.includes(current.source)" :historical="current !== state.report" />

      <div class="import-scroll">
        <ImportReport v-if="current && state.busy && !current.capture" :current="current" busy :recent="state.recent" />
        <section class="import-section" aria-labelledby="timecut-destination">
          <h2 id="timecut-destination">{{ t('timecut.destination') }}</h2>
          <form v-if="editingRoot || !state.target" class="section-form" @submit.prevent="saveTarget">
            <label for="timecut-root" class="help">{{ t('timecut.destinationHint') }}</label>
            <div class="path-row">
              <input id="timecut-root" v-model="root" class="path-input" spellcheck="false" autocomplete="off"
                :placeholder="t('timecut.pathPlaceholder')" />
              <button class="secondary-button" type="button" :disabled="!!pending" @click="browse('root')">{{ t('timecut.browse') }}</button>
            </div>
            <div class="form-actions">
              <button v-if="state.target" class="quiet-button" type="button" :disabled="!!pending" @click="cancelEdit">{{ t('timecut.cancel') }}</button>
              <button class="primary-button" type="submit" data-testid="target-save" :disabled="!root.trim() || !!pending">
                {{ t('timecut.save') }}
              </button>
            </div>
          </form>
          <div v-else class="saved-path">
            <IconFolder aria-hidden="true" /><span :title="state.target.root">{{ state.target.root }}</span>
            <button class="quiet-button" data-testid="target-edit" type="button" @click="editTarget">{{ t('timecut.change') }}</button>
          </div>
          <p v-if="feedback?.at === 'root'" :role="feedback.error ? 'alert' : 'status'" :class="['feedback', { problem: feedback.error }]">{{ feedback.text }}</p>
        </section>

        <section class="import-section" aria-labelledby="timecut-phone">
          <div class="section-heading">
            <h2 id="timecut-phone">{{ t('timecut.phone') }}</h2>
            <label class="switch-label">
              <input v-model="auto" type="checkbox" role="switch" class="toggle toggle-sm toggle-primary" :disabled="!!pending" @change="changeAuto" />
              <span>{{ t('timecut.auto') }}</span>
            </label>
          </div>
          <div class="device-row">
            <span data-testid="devices" role="status">{{ deviceText }}</span>
            <button class="quiet-button" type="button" :disabled="!!pending || !state.target" @click="retry">
              <IconRefresh aria-hidden="true" />{{ t('timecut.detect') }}
            </button>
          </div>
          <p class="help">{{ t('timecut.phoneHint') }}</p>
          <p v-if="state.device_error" class="feedback problem" role="alert">{{ state.device_error }}</p>
          <p v-if="feedback?.at === 'phone'" :role="feedback.error ? 'alert' : 'status'" :class="['feedback', { problem: feedback.error }]">{{ feedback.text }}</p>
        </section>

        <section class="import-section" aria-labelledby="timecut-local">
          <h2 id="timecut-local"><label for="timecut-source">{{ t('timecut.folder') }}</label></h2>
          <form class="section-form" @submit.prevent="submitFolder">
            <div class="path-row">
              <input id="timecut-source" v-model="source" class="path-input" spellcheck="false" autocomplete="off"
                :placeholder="t('timecut.sourcePlaceholder')" />
              <button class="secondary-button" type="button" :disabled="!!pending" @click="browse('folder')">{{ t('timecut.browse') }}</button>
            </div>
            <div class="mode-row">
              <select v-model="mode" class="mode-select" data-testid="import-mode" :aria-label="t('timecut.mode')">
                <option value="copy">{{ t('timecut.copy') }}</option>
                <option value="index">{{ t('timecut.index') }}</option>
              </select>
              <button class="primary-button" type="submit" data-testid="folder-start" :disabled="!canSubmit">
                {{ mode === 'index' ? t('timecut.addAlbum') : t('timecut.start') }}
              </button>
            </div>
            <p class="help">{{ folderHint }}</p>
          </form>
          <p v-if="feedback?.at === 'folder'" :role="feedback.error ? 'alert' : 'status'" :class="['feedback', { problem: feedback.error }]">{{ feedback.text }}</p>
        </section>

        <details v-if="current?.capture" class="phone-details">
          <summary>{{ t('timecut.transferDetails') }}</summary>
          <ImportReport :current="current" :busy="state.busy" :recent="state.recent" :live-release="liveRelease"
            :retry-at="state.target?.auto ? (state.apple_retry_at ?? state.next_retry_at) : null" />
        </details>
        <ImportReport v-else-if="current && !state.busy" :current="current" :recent="state.recent"
          :retry-at="state.target?.auto ? (state.apple_retry_at ?? state.next_retry_at) : null" />
        <p v-if="serviceError" class="feedback problem service-error" role="alert">{{ serviceError }}</p>
      </div>
    </dialog>
  </Teleport>
</template>

<script setup>
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { IconClose, IconFolder, IconRefresh, IconUpload } from '@/common/icons';
import { openFolderDialog } from '@/common/utils';
import { useTimecutIndex } from '@/composables/useTimecutIndex';
import { useUIStore } from '@/stores/uiStore';
import ImportReport from './TimecutImportReport.vue';
import PhoneStatus from './TimecutPhoneStatus.vue';

const { t, n } = useI18n();
const uiStore = useUIStore();
const state = ref({ target: null, devices: [], report: {}, recent: [], busy: false });
const root = ref(''), source = ref(''), mode = ref('copy'), auto = ref(true);
const opened = ref(false), editingRoot = ref(false), pending = ref('');
const feedback = ref(null), statusError = ref(''), indexError = ref('');
const serviceError = computed(() => statusError.value || indexError.value);
const opener = ref(null), dialog = ref(null);
const recentRelease = ref(null);
const current = computed(() => state.value.report?.phase ? state.value.report : state.value.busy ? null : state.value.recent[0]);
const liveRelease = computed(() => {
  const report = state.value.report, release = recentRelease.value, target = state.value.target;
  return !!release && !report.local_errors && (!report.phone_channel || report.windows_export?.complete)
    && report.safe_at === release.safe_at && report.device_key === release.device_key
    && report.source === release.source && target?.library === release.library && target?.album === release.album
    && target?.root === release.root && state.value.devices.length === 1 && state.value.devices[0] === report.source;
});
const importLabel = computed(() => {
  if (state.value.report.local_errors) return t('timecut.localRepair');
  if (liveRelease.value) return t('timecut.safeToUnplug');
  if (state.value.report.phone_channel === 'apple_camera' && state.value.report.safe_at
    && state.value.devices.includes(state.value.report.source)) return t('timecut.appleOriginalsComplete');
  if (!state.value.busy) return t('timecut.title');
  if (state.value.report.phase === '正在本地整理') return t('timecut.localFiling');
  const report = state.value.report;
  const completed = (report.added ?? 0) + (report.skipped ?? 0);
  if (completed) return t('timecut.importingCount', { count: n(completed) });
  if (report.discovered) return t('timecut.discoveredCount', { count: n(report.discovered) });
  return t(report.scanned ? 'timecut.reading' : 'timecut.checking');
});
const unsavedDestination = computed(() => !state.value.target || (editingRoot.value && root.value.trim() !== state.value.target.root));
const canSubmit = computed(() => !pending.value && !!source.value.trim() && (mode.value === 'index' || !unsavedDestination.value));
const folderHint = computed(() => mode.value === 'index' ? t('timecut.indexHint') : unsavedDestination.value ? t('timecut.saveFirst') : t('timecut.copyHint'));
const deviceText = computed(() => {
  if (state.value.devices.length) return t('timecut.connected', { names: state.value.devices.join(t('timecut.separator')) });
  if (!auto.value) return t('timecut.off');
  if (state.value.apple_retry_at) return t('timecut.appleWaitingAccess');
  return state.value.target ? t('timecut.waiting') : t('timecut.afterSave');
});

let first = true, disposed = false, timer, statusRequest, releaseTimer, unlistenRelease;
useTimecutIndex(error => { indexError.value = error == null ? '' : String(error); });

function close() {
  dialog.value?.close();
  opened.value = false;
  nextTick(() => opener.value?.focus({ preventScroll: true }));
}
watch(dialog, element => {
  uiStore.removeInputHandler('TimecutImport');
  if (!element) return;
  uiStore.pushInputHandler('TimecutImport');
  element.showModal();
  element.querySelector('h1')?.focus({ preventScroll: true });
}, { flush: 'post' });

function keepTabInDialog(event) {
  const items = Array.from(dialog.value.querySelectorAll('button:enabled, input:enabled, select:enabled, summary'))
    .filter(element => element.getClientRects().length);
  const next = event.shiftKey ? items.at(-1) : items[0];
  if (next && (event.shiftKey ? document.activeElement === items[0] || document.activeElement?.tagName === 'H1' : document.activeElement === items.at(-1))) {
    event.preventDefault();
    next.focus();
  }
}

async function readStatus() {
  if (statusRequest) return statusRequest;
  statusRequest = invoke('timecut_status').then(next => {
    if (disposed) return;
    const old = state.value.target;
    if (recentRelease.value && (next.target?.library !== old?.library || next.target?.album !== old?.album
      || next.target?.root !== old?.root || !next.devices.includes(recentRelease.value.source))) recentRelease.value = null;
    state.value = next;
    if (first) {
      root.value = next.target?.root || '';
      auto.value = next.target?.auto ?? true;
      editingRoot.value = !next.target;
      opened.value = !next.target;
      first = false;
    } else if (old?.root !== next.target?.root || old?.auto !== next.target?.auto) {
      if (!editingRoot.value || root.value === old?.root) root.value = next.target?.root || '';
      if (!pending.value) auto.value = next.target?.auto ?? true;
      if (!next.target) editingRoot.value = true;
    }
    statusError.value = '';
  }).catch(error => { statusError.value = String(error); }).finally(() => { statusRequest = undefined; });
  return statusRequest;
}

async function poll() {
  await readStatus();
  if (!disposed) timer = setTimeout(poll, state.value.busy ? 500 : opened.value || state.value.target?.auto ? 1500 : 8000);
}
watch(opened, value => { if (value) void readStatus(); });
onMounted(async () => {
  void poll();
  try {
    const remove = await listen('timecut-originals-safe', ({ payload }) => {
      const target = state.value.target;
      if (disposed || !target || !payload?.safe_at || !payload.device_key) return;
      recentRelease.value = { safe_at: payload.safe_at, source: payload.source, device_key: payload.device_key,
        library: target.library, album: target.album, root: target.root };
      clearTimeout(releaseTimer);
      releaseTimer = setTimeout(() => { recentRelease.value = null; }, 10_000);
      void readStatus();
    });
    if (disposed) remove();
    else unlistenRelease = remove;
  } catch (error) { if (!disposed) statusError.value = String(error); }
});
onUnmounted(() => {
  disposed = true;
  clearTimeout(timer);
  clearTimeout(releaseTimer);
  unlistenRelease?.();
  uiStore.removeInputHandler('TimecutImport');
});

async function action(at, operation, success) {
  if (pending.value) return false;
  pending.value = at;
  feedback.value = null;
  try {
    await operation();
    if (statusRequest) await statusRequest;
    await readStatus();
    feedback.value = { at, text: success, error: false };
    return true;
  } catch (error) {
    feedback.value = { at, text: String(error), error: true };
    return false;
  } finally { pending.value = ''; }
}

async function browse(at) {
  try {
    const path = await openFolderDialog(t(at === 'root' ? 'timecut.chooseDestination' : 'timecut.chooseSource'));
    if (path) (at === 'root' ? root : source).value = String(path);
  } catch (error) { feedback.value = { at, text: String(error), error: true }; }
}
function editTarget() {
  feedback.value = null;
  root.value = state.value.target.root;
  editingRoot.value = true;
  nextTick(() => document.getElementById('timecut-root')?.focus());
}
function cancelEdit() {
  root.value = state.value.target.root;
  auto.value = state.value.target.auto;
  editingRoot.value = false;
  feedback.value = null;
}
async function saveTarget() {
  if (!root.value.trim()) return;
  if (await action('root', () => invoke('timecut_configure', { path: root.value.trim(), auto: auto.value }), t('timecut.saved'))) {
    editingRoot.value = false;
  }
}
async function changeAuto() {
  if (!state.value.target) return;
  if (!await action('phone', () => invoke('timecut_set_auto', { auto: auto.value }), t(auto.value ? 'timecut.enabled' : 'timecut.disabled'))) {
    auto.value = state.value.target?.auto ?? true;
  }
}
function retry() { return action('phone', () => invoke('timecut_retry'), t('timecut.detecting')); }
function submitFolder() {
  if (!canSubmit.value) return;
  const indexOnly = mode.value === 'index';
  return action('folder', () => invoke('timecut_folder', { path: source.value.trim(), indexOnly }), t(indexOnly ? 'timecut.indexQueued' : 'timecut.copyQueued'));
}
</script>

<style scoped>
.import-entry, .close-button, .quiet-button, .secondary-button, .primary-button {
  display: inline-flex; align-items: center; justify-content: center; gap: .45rem;
  font: inherit; font-size: .8125rem; font-weight: 600; cursor: pointer;
}
.import-entry {
  min-height: 1.85rem; padding: 0 .75rem; border: 1px solid color-mix(in srgb, var(--color-base-content) 24%, transparent);
  border-radius: var(--radius-field, .4rem); background: var(--color-base-200); color: var(--color-base-content);
}
.import-entry:hover { background: var(--color-base-100); }
.import-entry svg, .quiet-button svg { width: 1rem; height: 1rem; flex-shrink: 0; }
.import-entry:focus-visible, .import-dialog :is(button, input, select, summary):focus-visible {
  outline: 2px solid var(--color-primary); outline-offset: 2px;
}
.import-dialog {
  --line: color-mix(in srgb, var(--color-base-content) 17%, var(--color-base-100));
  --soft-text: color-mix(in srgb, var(--color-base-content) 79%, var(--color-base-100));
  margin: auto; padding: 0; inline-size: min(37rem, calc(100% - 1.5rem)); max-inline-size: 100%;
  max-block-size: calc(100dvh - 1.5rem); border: 1px solid var(--line);
  border-radius: .85rem; background: var(--color-base-100); color: var(--color-base-content);
  overflow: hidden; font-size: .875rem; line-height: 1.5;
}
.import-dialog[open] { display: flex; flex-direction: column; }
.import-dialog::backdrop { background: rgb(0 0 0 / .48); }
.import-header, .section-heading, .device-row, .mode-row, .form-actions, .saved-path { display: flex; align-items: center; gap: .65rem; }
.import-header { flex-shrink: 0; justify-content: space-between; padding: .9rem 1.35rem; border-bottom: 1px solid var(--line); }
.import-header h1 { margin: 0; font-size: 1.25rem; font-weight: 650; line-height: 1.35; outline: none; }
.close-button { width: 2rem; height: 2rem; border-radius: .4rem; color: var(--soft-text); }
.close-button svg { width: 1.15rem; height: 1.15rem; }
.close-button:hover, .quiet-button:hover { background: var(--color-base-200); color: var(--color-base-content); }
.import-scroll { overflow: auto; min-height: 0; overscroll-behavior: contain; scrollbar-gutter: stable; }
.import-section { margin-inline: 1.35rem; padding-block: 1rem; border-bottom: 1px solid var(--line); }
.import-section:last-child { border: none; }
.import-section h2 { font-size: .98rem; font-weight: 650; }
.phone-details > summary {
  margin-inline: 1.35rem;
  padding-block: .7rem;
  font-weight: 600;
  cursor: pointer;
}
.section-heading, .device-row, .mode-row { justify-content: space-between; flex-wrap: wrap; }
.section-form { display: grid; gap: .55rem; margin-top: .5rem; }
.help, .feedback, .device-row { font-size: .8125rem; }
.help { color: var(--soft-text); }
.path-row { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: .5rem; }
.path-input, .mode-select, .secondary-button, .primary-button {
  min-height: 2.1rem; border-radius: .4rem; font-size: .8125rem;
}
.path-input, .mode-select, .secondary-button { border: 1px solid color-mix(in srgb, var(--color-base-content) 35%, var(--color-base-100)); }
.path-input, .mode-select { background: var(--color-base-100); color: var(--color-base-content); padding: .25rem .65rem; }
.path-input { min-width: 0; width: 100%; user-select: text; }
.path-input::placeholder { color: var(--soft-text); }
.mode-select { max-width: 100%; cursor: pointer; }
.secondary-button, .primary-button { padding-inline: .85rem; white-space: nowrap; }
.secondary-button { background: var(--color-base-200); }
.secondary-button:hover { background: var(--color-base-300); }
.primary-button { background: var(--color-primary); color: var(--color-primary-content); }
.primary-button:hover { background: color-mix(in srgb, var(--color-primary) 90%, var(--color-base-content)); }
.import-dialog button:disabled { cursor: default; color: var(--soft-text); background: var(--color-base-200); }
.form-actions { justify-content: flex-end; }
.quiet-button { min-height: 2rem; padding-inline: .45rem; border-radius: .4rem; font-weight: 500; }
.saved-path { margin-top: .5rem; color: var(--soft-text); }
.saved-path > svg { width: 1.1rem; height: 1.1rem; flex-shrink: 0; }
.saved-path > span { flex: 1; min-width: 0; overflow-wrap: anywhere; user-select: text; }
.saved-path .quiet-button { flex-shrink: 0; color: var(--color-base-content); }
.switch-label { display: inline-flex; align-items: center; gap: .5rem; min-height: 2rem; cursor: pointer; font-size: .8125rem; }
.device-row { margin-block: .15rem; color: var(--soft-text); }
.device-row > span { overflow-wrap: anywhere; }
.feedback { margin-top: .45rem; overflow-wrap: anywhere; color: var(--color-base-content); user-select: text; }
.problem { color: color-mix(in srgb, var(--color-error) 59%, var(--color-base-content)); }
.service-error { margin: .65rem 1.35rem; }
@media (max-width: 32rem), (max-height: 35rem) {
  .import-header { padding: .65rem .9rem; }
  .import-section, .phone-details > summary { margin-inline: .9rem; padding-block: .7rem; }
}
</style>
