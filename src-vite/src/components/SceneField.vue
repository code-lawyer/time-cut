<template>
  <div class="scene-field pl-4 pr-1 grid min-w-0 gap-y-1">
    <label :for="fieldId" class="text-xs text-base-content/70">{{ t('scene.title') }}</label>
    <select :id="fieldId" data-testid="scene-file-category" class="select select-xs w-full min-w-0 max-w-full focus-visible:outline-primary"
      :title="t(`scene.${scene?.manual ?? scene?.automatic ?? 'pending'}`)"
      :disabled="saving || !scene" :value="scene?.manual ?? ''" @change="change">
      <option value="">{{ t('scene.automaticLabel', { category: t(`scene.${scene?.automatic ?? 'pending'}`) }) }}</option>
      <option v-for="category in SCENES" :key="category" :value="category">{{ t(`scene.${category}`) }}</option>
    </select>
    <details :key="`${library}:${fileId}`" class="scene-agent col-span-full" @toggle="loadProviders">
      <summary data-testid="scene-agent-open">{{ t('scene.agentTitle') }}</summary>
      <p v-if="!providers" role="status">{{ t('scene.agentLoading') }}</p>
      <p v-else-if="providers.length === 0">{{ t('scene.agentUnavailable') }}</p>
      <template v-else>
        <p>{{ t('scene.agentDisclosure') }}</p>
        <label class="sr-only" :for="providerId">{{ t('scene.agentTool') }}</label>
        <select :id="providerId" v-model="selected" class="select select-sm w-full">
          <option v-for="provider in providers" :key="provider" :value="provider">{{ providerNames[provider] }}</option>
        </select>
        <button type="button" class="btn btn-sm btn-outline" :disabled="sending || !selected"
          data-testid="scene-agent-send" @click="requestSuggestion">
          {{ t(sending ? 'scene.agentSending' : 'scene.agentSend') }}
        </button>
        <div v-if="suggestion" class="scene-suggestion">
          <span role="status">{{ t('scene.agentResult', { category: t(`scene.${suggestion}`) }) }}</span>
          <button type="button" class="btn btn-sm btn-primary" :disabled="saving" @click="save(suggestion)">
            {{ t('scene.agentApply') }}
          </button>
        </div>
      </template>
      <p v-if="providerError" class="text-error" role="alert">{{ providerError }}</p>
    </details>
    <p v-if="error" class="col-span-full text-error text-xs break-words" role="alert">{{ error }}</p>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref, useId, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import {
  SCENES, getFileScene, setFileScene, getSceneProviders, suggestScene,
  type FileScene, type Scene, type SceneProvider,
} from '@/common/timecutScenes';

const props = defineProps<{ fileId: number; library: string }>();
const fieldId = useId();
const providerId = useId();
const { t } = useI18n();
const scene = ref<FileScene | null>(null);
const error = ref('');
const saving = ref(false);
const sending = ref(false);
const providers = ref<SceneProvider[] | null>(null);
const selected = ref<SceneProvider | null>(null);
const providerError = ref('');
const suggestion = ref<Scene | null>(null);
const providerNames = { codex: 'Codex', claude: 'Claude Code' };
let unlisten: UnlistenFn | undefined;
let active = true;
let selection = 0;
let request = 0;

async function refresh() {
  const id = ++request;
  const { library, fileId } = props;
  try {
    const result = await getFileScene(library, fileId);
    if (active && id === request && props.fileId === fileId && props.library === library) scene.value = result;
  } catch (reason) {
    if (id === request && props.fileId === fileId && props.library === library) error.value = String(reason);
  }
}

function change(event: Event) {
  const value = (event.currentTarget as HTMLSelectElement).value as Scene | '';
  void save(value || null);
}

async function save(category: Scene | null) {
  const { library, fileId } = props;
  const id = selection;
  saving.value = true;
  error.value = '';
  try {
    await setFileScene(library, fileId, category);
    if (active && id === selection) {
      suggestion.value = null;
      await refresh();
    }
  } catch (reason) {
    if (active && id === selection) error.value = String(reason);
  } finally { if (id === selection) saving.value = false; }
}

async function loadProviders(event: Event) {
  if (!(event.currentTarget as HTMLDetailsElement).open || providers.value !== null) return;
  try {
    providers.value = await getSceneProviders();
    selected.value = providers.value[0] ?? null;
  } catch (reason) {
    providers.value = [];
    providerError.value = String(reason);
  }
}

async function requestSuggestion() {
  if (!selected.value || sending.value) return;
  const { library, fileId } = props;
  const id = selection;
  sending.value = true;
  suggestion.value = null;
  providerError.value = '';
  try {
    const result = await suggestScene(library, fileId, selected.value);
    if (active && id === selection) suggestion.value = result;
  } catch (reason) {
    if (active && id === selection) providerError.value = String(reason);
  } finally { if (id === selection) sending.value = false; }
}

watch(() => [props.fileId, props.library], () => {
  selection++;
  scene.value = null;
  suggestion.value = null;
  saving.value = sending.value = false;
  error.value = providerError.value = '';
  void refresh();
}, { immediate: true });
onMounted(async () => {
  try {
    const remove = await listen<string>('timecut-scenes-changed', ({ payload }) => {
      if (payload === props.library && !saving.value) void refresh();
    });
    if (active) unlisten = remove;
    else remove();
  } catch (reason) { if (active) error.value = String(reason); }
});
onUnmounted(() => { active = false; unlisten?.(); });
</script>

<style scoped>
.scene-agent { padding: .25rem 0; display: grid; gap: .45rem; font-size: .75rem; min-width: 0; }
.scene-agent summary { cursor: pointer; width: fit-content; }
.scene-agent p { margin: .45rem 0; line-height: 1.45; overflow-wrap: anywhere; }
.scene-agent select { margin: .15rem 0 .45rem; }
.scene-agent :is(summary, select, button):focus-visible { outline: 2px solid var(--color-primary); outline-offset: 2px; }
.scene-suggestion { margin-top: .5rem; display: flex; flex-wrap: wrap; gap: .45rem; align-items: center; }
</style>
