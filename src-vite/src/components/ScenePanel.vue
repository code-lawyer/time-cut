<template>
  <section class="sidebar-panel" aria-labelledby="scene-title">
    <header class="sidebar-panel-header">
      <h2 id="scene-title" class="sidebar-panel-header-title">{{ t('scene.title') }}</h2>
    </header>
    <div class="scene-scroll">
      <button v-for="item in filters" :key="item || 'all'" type="button" :aria-pressed="lib.scenes.category === item"
        :class="['sidebar-item', 'scene-item', lib.scenes.category === item ? 'sidebar-item-selected' : 'sidebar-item-hover']"
        :data-testid="`scene-filter-${item ?? 'all'}`" @click="select(item)">
        <span class="sidebar-item-label">{{ t(`scene.${item ?? 'all'}`) }}</span>
        <span v-if="status" class="sidebar-item-count">{{ n(item ? status.counts[item] ?? 0 : total) }}</span>
      </button>
    </div>
    <div class="scene-settings">
      <label class="scene-toggle">
        <span>{{ t('scene.auto') }}</span>
        <input type="checkbox" class="toggle toggle-sm toggle-primary" :checked="status?.automatic"
          :disabled="changing || !status" @change="toggleAutomatic" />
      </label>
      <p v-if="status" role="status" class="scene-feedback">
        {{ !status.automatic ? t('scene.paused') : status.running && pending > 0 ? t('scene.running', { count: n(pending) }) : t('scene.local') }}
      </p>
      <p v-if="status && (status.unavailable || status.failed)" class="scene-feedback">
        {{ t('scene.incomplete', { offline: n(status.unavailable), failed: n(status.failed) }) }}
      </p>
      <p v-if="error || status?.error" role="alert" class="scene-feedback scene-error">{{ error || status?.error }}</p>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useLibraryStore } from '@/stores/libraryStore';
import { SCENES, sceneStatus, type SceneFilter, type SceneStatus } from '@/common/timecutScenes';

const { t, n } = useI18n();
const lib = useLibraryStore();
const status = ref<SceneStatus | null>(null);
const changing = ref(false);
const error = ref('');
const total = computed(() => Object.values(status.value?.counts ?? {}).reduce((sum, value) => sum + value, 0));
const pending = computed(() => status.value?.counts.pending ?? 0);
const filters: (SceneFilter | null)[] = [null, ...SCENES, 'pending'];
let unlisten: UnlistenFn | undefined;
let timer: ReturnType<typeof setInterval> | undefined;
let active = true;
let request = 0;

async function refresh() {
  const library = lib._libraryId;
  const id = ++request;
  if (!library) return;
  try {
    const result = await sceneStatus(library);
    if (active && lib._libraryId === library && id === request) {
      status.value = result;
      error.value = '';
    }
  } catch (reason) {
    if (active && lib._libraryId === library && id === request) error.value = String(reason);
  }
}

function select(item: SceneFilter | null) {
  lib.activePane = 'main';
  lib.scenes.category = item;
}

async function toggleAutomatic(event: Event) {
  const library = lib._libraryId;
  if (!library) return;
  const enabled = (event.currentTarget as HTMLInputElement).checked;
  changing.value = true;
  try {
    await invoke('timecut_set_scene_auto', { library, enabled });
    await refresh();
  } catch (reason) {
    if (active && lib._libraryId === library) error.value = String(reason);
  } finally { changing.value = false; }
}

watch(() => lib._libraryId, () => { status.value = null; void refresh(); });
onMounted(async () => {
  try {
    const unsubscribe = await listen<string>('timecut-scenes-changed', ({ payload }) => {
      if (payload === lib._libraryId) {
        lib.scenes.revision++;
        void refresh();
      }
    });
    if (!active) { unsubscribe(); return; }
    unlisten = unsubscribe;
  } catch (reason) { if (active) error.value = String(reason); }
  if (!active) return;
  void refresh();
  // The backend reuses counts for the same catalog revision.
  timer = setInterval(refresh, 5000);
});
onUnmounted(() => { active = false; clearInterval(timer); unlisten?.(); });
</script>

<style scoped>
.scene-scroll { flex: 1; min-height: 0; overflow: auto; padding: .2rem .3rem .55rem; }
.scene-item { width: calc(100% - .5rem); min-height: 2rem; text-align: start; margin: .1rem .25rem; }
.scene-item:focus-visible, .scene-toggle input:focus-visible { outline: 2px solid var(--color-primary); outline-offset: 1px; }
.scene-settings { margin: .15rem .55rem .6rem; border-top: 1px solid color-mix(in srgb, var(--color-base-content) 16%, transparent); padding-top: .65rem; }
.scene-toggle { display: flex; align-items: center; justify-content: space-between; gap: .5rem; font-size: .8125rem; }
.scene-feedback { margin: .45rem 0 0; color: color-mix(in srgb, var(--color-base-content) 74%, var(--color-base-100)); font-size: .75rem; line-height: 1.45; overflow-wrap: anywhere; }
.scene-error { color: color-mix(in srgb, var(--color-error) 70%, var(--color-base-content)); }
</style>
