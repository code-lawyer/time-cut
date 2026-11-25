<template>
  <div class="sidebar-panel overflow-hidden">
    <div v-if="isIndexing" class="face-status">
      <div role="status" class="min-w-0">
        <span>
          {{ indexProgress.phase === 'clustering' 
            ? $t('face_index.clustering') 
            : $t('face_index.indexing', { current: indexProgress.current.toLocaleString(), total: indexProgress.total.toLocaleString() }) 
          }}
        </span>
        <span v-if="indexProgress.phase === 'clustering' && clusterProgressText" class="block">
          {{ clusterProgressText }}
        </span>
        <span v-else-if="indexProgress.faces_found > 0" class="block">
          {{ $t('face_index.faces_found', { count: indexProgress.faces_found.toLocaleString() }) }}
        </span>
      </div>
      <button type="button" class="btn btn-xs" @click="clickCancelIndex">{{ $t('face_index.cancel') }}</button>
    </div>
    <p v-if="indexError && !isIndexing" role="alert" class="face-status">{{ indexError }}</p>

    <!-- Incomplete Indexing Warning Banner -->
    <div v-if="allPersons.length > 0 && incompleteCount > 0 && !isIndexing" class="flex-none px-2 py-2">
        <div class="p-3 rounded-box flex flex-row items-center gap-2">
          <IconUpdate class="w-5 h-5 shrink-0" />
          <span class="text-xs flex-1">
            {{ $t('face_index.incomplete', { count: incompleteCount.toLocaleString() }) }}
          </span>
          <button class="btn btn-xs btn-primary" @click="clickIndexFaces">
            {{ $t('face_index.resume') }}
          </button>
        </div>
    </div>

    <div class="sidebar-panel-header">
      <span class="sidebar-panel-header-title flex-1 min-w-0 overflow-hidden text-ellipsis whitespace-nowrap">
        {{ titlebar }}<template v-if="allPersonCount > 0"> ({{ allPersonCount.toLocaleString() }})</template>
      </span>
      <SortMenuButton v-model="config.settings.personSort" kind="category" />
      <ContextMenu :menuItems="personPanelMenuItems" :iconMenu="IconMore" :smallIcon="true" />
    </div>

    <div class="mx-1 mb-2 px-1 shrink-0">
      <div
        :class="[
          'h-8 flex items-center rounded-box transition-colors bg-base-100/40',
          isPersonSearchFocused ? 'border-2 border-primary' : 'border border-base-content/10 hover:border-base-content/30',
          personSearchDisabled ? 'opacity-50' : '',
        ]"
      >
        <IconSearch class="ml-2 w-4 h-4 shrink-0" :class="isPersonSearchFocused ? 'text-primary/70' : 'text-base-content/30'" />
        <input
          v-model="personSearch"
          type="text"
          :aria-label="$t('menu.person.search')"
          :disabled="personSearchDisabled"
          :placeholder="$t('menu.person.search')"
          class="w-full min-w-0 bg-transparent border-none focus:ring-0 px-2 text-sm placeholder-base-content/80 focus:outline-none disabled:opacity-50"
          @focus="isPersonSearchFocused = true"
          @blur="isPersonSearchFocused = false"
        />
        <button
          v-if="personSearch"
          type="button"
          :aria-label="$t('timecut.clearSearch')"
          :disabled="personSearchDisabled"
          class="mr-1 p-1 rounded-box text-base-content/30 hover:text-base-content/70 disabled:opacity-30"
          @click="personSearch = ''"
        >
          <IconClose class="w-4 h-4" />
        </button>
      </div>
    </div>

    <!-- Person List -->
    <div
      v-if="allPersons.length > 0"
      class="grow overflow-x-hidden overflow-y-auto"
      @scroll="handlePersonListScroll"
    >
      <ul>
        <li v-for="person in sortedPersons" :key="person.id" :id="'person-' + person.id">
          <div
            :class="[
              'sidebar-item gap-2 group',
              selectedPerson && selectedPerson.id === person.id && !isRenamingPerson ? 'sidebar-item-selected' : 'sidebar-item-hover',
            ]"
            @click="selectPerson(person)"
            @contextmenu.prevent.stop="(e: MouseEvent) => handlePersonContextMenu(person, e)"
          >
            <div class="w-8 h-8 rounded-full overflow-hidden bg-base-300/70 ring-1 ring-base-content/5 shrink-0 flex items-center justify-center">
              <img 
                v-if="person.thumbnail" 
                :src="'data:image/jpeg;base64,' + person.thumbnail" 
                alt="" class="w-full h-full object-cover"
              />
              <IconPerson v-else class="w-5 h-5 text-base-content/30" />
            </div>
            
            <!-- Name input or display -->
            <input v-if="selectedPerson && selectedPerson.id === person.id && isRenamingPerson"
              ref="personInputRef"
              type="text"
              maxlength="255"
              class="input px-1 flex-1 focus:border text-base"
              v-model="person.name"
              @keydown.enter="handleRenamePerson"
              @keydown.esc="cancelRenamePerson"
              @blur="handleRenamePerson"
            />
            <template v-else>
              <button type="button" class="sidebar-item-label h-full text-start rounded focus-visible:outline-primary"
                :aria-pressed="selectedPerson?.id === person.id" @click.stop="selectPerson(person)">
                {{ getPersonDisplayName(person) }}
              </button>
              <div class="ml-auto flex flex-row items-center text-base-content/30">
                <span v-if="person.count" class="sidebar-item-count shrink-0">
                  {{ person.count.toLocaleString() }}
                </span>
                <div :class="[
                    selectedPerson?.id === person.id ? '' : 'hidden group-hover:flex'
                  ]"
                >
                  <ContextMenu
                    :ref="(el: any) => { if (el) personContextMenus[person.id] = el }"
                    :iconMenu="IconMore"
                    :menuItems="getMoreMenuItems()"
                    :smallIcon="true"
                  />
                </div>
              </div>
            </template>
          </div>
        </li>
      </ul>
    </div>

    <div v-else-if="isLoadingPersons" class="mt-2 px-2 flex flex-col items-center justify-center text-base-content/30">
      <span class="text-sm text-center">{{ $t('tooltip.loading') }}</span>
    </div>

    <div v-else-if="personSearch" class="sidebar-empty text-sm">
      <span class="text-center">{{ $t('tooltip.not_found.person') }}</span>
    </div>

    <!-- No Persons Found Message -->
    <div v-else-if="!isIndexing && incompleteCount > 0" class="mt-2 px-2 flex flex-col items-center justify-center text-base-content/30">
      <span class="text-sm text-center">{{ $t('face_index.incomplete', { count: incompleteCount.toLocaleString() }) }}</span>
      <button class="btn btn-primary btn-sm mt-4 rounded-box" @click="clickIndexFaces">
        <IconUpdate class="w-4 h-4" />
        {{ $t('face_index.resume') }}
      </button>
    </div>

    <div v-else-if="!isIndexing" class="mt-2 px-2 flex flex-col items-center justify-center text-base-content/30">
      <span class="text-sm text-center">{{ $t('tooltip.not_found.person') }}</span>
    </div>

  </div>

  <!-- Delete person confirmation -->
  <MessageBox
    v-if="showDeletePersonMsgbox"
    :title="$t('msgbox.delete_person.title')"
    :message="`${$t('msgbox.delete_person.content', { person: getPersonDisplayName(selectedPerson) })}`"
    :OkText="$t('msgbox.delete_person.ok')"
    :cancelText="$t('msgbox.cancel')"
    :warningOk="true"
    @ok="clickDeletePerson"
    @cancel="showDeletePersonMsgbox = false"
  />

  <!-- Reset faces confirmation -->
  <MessageBox
    v-if="showResetFacesMsgbox"
    :title="$t('msgbox.reset_faces.title')"
    :message="$t('msgbox.reset_faces.content')"
    :OkText="$t('msgbox.reset_faces.ok')"
    :cancelText="$t('msgbox.cancel')"
    :warningOk="true"
    @ok="onResetFacesConfirm"
    @cancel="showResetFacesMsgbox = false"
  />

</template>

<script setup lang="ts">
import { ref, onMounted, onUnmounted, computed, nextTick, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { config, libConfig } from '@/common/config';
import { getPersonsPage, renamePerson, deletePerson, indexFaces, cancelFaceIndex, isFaceIndexing, listenFaceIndexProgress, listenFaceIndexFinished, listenClusterProgress, resetFaces, getFaceStats } from '@/common/api';
import { useLibraryChangeRefresh } from '@/composables/useLibraryChangeRefresh';
import { SIDEBAR, PERSON_PAGE_SIZE } from '@/common/constants';
import { 
  IconPerson, 
  IconMore, 
  IconRename, 
  IconTrash,
  IconUpdate,
  IconClose,
  IconSearch,
} from '@/common/icons';

import ContextMenu from '@/components/ContextMenu.vue';
import MessageBox from '@/components/MessageBox.vue';
import SortMenuButton from '@/components/SortMenuButton.vue';

const props = defineProps({
  titlebar: {
    type: String,
    required: true
  }
});

const emit = defineEmits(['editDataChanged']);

/// i18n
const { locale, messages } = useI18n();
const localeMsg = computed(() => messages.value[locale.value] as any);

// persons
const allPersons = ref<any[]>([]);
const selectedPerson = ref<any>(null);
const isRenamingPerson = ref(false);
const originalPersonName = ref('');
const personInputRef = ref<HTMLInputElement[]>([]);
const isIndexing = ref(false);
const indexProgress = ref({
  current: 0,
  total: 0,
  faces_found: 0,
  phase: 'indexing'
});
const clusterProgress = ref({
  phase: '',
  current: 0,
  total: 0
});
const incompleteCount = ref(0);
const personContextMenus = ref<Record<number, any>>({});
const isLoadingPersons = ref(true);
const isLoadingMorePersons = ref(false);
const hasMorePersons = ref(false);
const allPersonCount = ref(0);
const personSearch = ref('');
const isPersonSearchFocused = ref(false);
// Disable search only for a truly empty library; never lock it on a zero-match
// search (allPersonCount now reflects the query), so the user can always clear it.
const personSearchDisabled = computed(
  () => !isLoadingPersons.value && allPersonCount.value === 0 && !personSearch.value,
);

let personLoadRequest = 0;
let personSearchTimer: ReturnType<typeof setTimeout> | null = null;
let isPersonMounted = true;

function handlePersonContextMenu(person: any, event: MouseEvent) {
  selectPerson(person);
  personContextMenus.value[person.id]?.open?.(event.clientX, event.clientY);
}
const indexError = ref('');

// Event listener unsubscribe functions
let unlistenProgress: (() => void) | null = null;
let unlistenFinished: (() => void) | null = null;
let unlistenCluster: (() => void) | null = null;

const sortedPersons = computed(() => allPersons.value);

// Computed property to format cluster progress text using i18n
const { t } = useI18n();
const getPersonDisplayName = (person: any) => person?.name || t('menu.person.unnamed');
const clusterProgressText = computed(() => {
  const { phase, current, total } = clusterProgress.value;
  switch (phase) {
    case 'graph':
      return t('face_index.cluster_graph', { percent: current });
    case 'iterate':
      return t('face_index.cluster_iterate', { current, total });
    case 'converged':
      return t('face_index.cluster_converged', { current });
    case 'assign':
      return t('face_index.cluster_assign', { current, total });
    case 'thumbnail':
      return t('face_index.cluster_thumbnail');
    default:
      return '';
  }
});

const personPanelMenuItems = computed(() => [
  {
    label: localeMsg.value.menu.person.index_faces,
    icon: IconUpdate,
    action: () => clickIndexFaces(),
    disabled: isIndexing.value,
  },
  { label: "-", action: null },
  {
    label: localeMsg.value.menu.person.reset_index,
    icon: IconTrash,
    action: () => clickResetFaces(),
    disabled: isIndexing.value,
  },
]);

// message boxes
const showDeletePersonMsgbox = ref(false);
const showResetFacesMsgbox = ref(false);

// more menuitems
const getMoreMenuItems = () => [
  {
    label: localeMsg.value.menu?.person?.rename || 'Rename',
    icon: IconRename,
    action: () => {
      isRenamingPerson.value = true;
      originalPersonName.value = selectedPerson.value?.name || '';
      nextTick(() => {
        if (personInputRef.value && personInputRef.value[0]) {
          personInputRef.value[0].focus();
        }
      });
    }
  },
  { label: "-", action: null },
  {
    label: localeMsg.value.menu?.person?.delete || 'Delete',
    icon: IconTrash,
    action: () => {
      showDeletePersonMsgbox.value = true;
    },
  },
];

onMounted(async () => {
  loadPersons();
  checkFaceStats();
  
  // Check if indexing is already running and restore progress
  // Set up event listeners for face indexing progress
  unlistenProgress = await listenFaceIndexProgress((event: any) => {
    if (event.payload.library_id !== libConfig._libraryId) return;
    isIndexing.value = true;
    indexError.value = '';
    indexProgress.value = event.payload;
  });
  
  unlistenFinished = await listenFaceIndexFinished((event: any) => {
    if (event.payload.library_id !== libConfig._libraryId) return;
    isIndexing.value = false;
    indexError.value = event.payload.error ?? '';
    indexProgress.value = { current: 0, total: 0, faces_found: 0, phase: 'indexing' };
    clusterProgress.value = { phase: '', current: 0, total: 0 };
    loadPersons(); // Reload persons after indexing completes
    checkFaceStats();
  });
  
  // Listen for detailed clustering progress
  unlistenCluster = await listenClusterProgress((event: any) => {
    clusterProgress.value = event.payload;
  });
  if (!isPersonMounted) {
    unlistenProgress?.();
    unlistenFinished?.();
    unlistenCluster?.();
    return;
  }
  await restoreProgress();
});

async function restoreProgress() {
  const library = libConfig._libraryId;
  const [running, progress] = (await isFaceIndexing()) || [false, null];
  if (!isPersonMounted || library !== libConfig._libraryId) return;
  isIndexing.value = !!(running && progress?.library_id === library);
  if (isIndexing.value) indexProgress.value = progress;
}

watch(() => config.settings.personSort, () => {
  loadPersons();
});

watch(personSearch, () => {
  if (personSearchTimer) clearTimeout(personSearchTimer);
  personLoadRequest++;
  allPersons.value = [];
  hasMorePersons.value = false;
  isLoadingPersons.value = true;
  isLoadingMorePersons.value = false;
  personSearchTimer = setTimeout(() => {
    personSearchTimer = null;
    void loadPersons();
  }, 200);
});

onUnmounted(() => {
  isPersonMounted = false;
  personLoadRequest++;
  if (personSearchTimer) clearTimeout(personSearchTimer);
  if (unlistenProgress) unlistenProgress();
  if (unlistenFinished) unlistenFinished();
  if (unlistenCluster) unlistenCluster();
});

async function loadPersons(reset = true, validateSelectedPerson = false) {
  if (!reset && (!hasMorePersons.value || isLoadingMorePersons.value || isLoadingPersons.value)) return;

  const requestId = reset ? ++personLoadRequest : personLoadRequest;
  const libraryId = libConfig._libraryId;
  const search = personSearch.value.trim();
  if (reset) {
    isLoadingPersons.value = true;
    allPersons.value = [];
    hasMorePersons.value = false;
  } else {
    isLoadingMorePersons.value = true;
  }

  try {
    const page = await getPersonsPage({
      sort: config.settings.personSort,
      offset: reset ? 0 : allPersons.value.length,
      limit: PERSON_PAGE_SIZE,
      search,
      refreshSummary: validateSelectedPerson
        ? { selectedPersonId: libConfig.person?.id ?? null }
        : null,
    });
    if (!isPersonMounted || requestId !== personLoadRequest || libraryId !== libConfig._libraryId) return;

    if (page) {
      const selectedPersonWasFiltered = validateSelectedPerson && page.selected_person_visible === false;
      if (selectedPersonWasFiltered) {
        selectedPerson.value = null;
        if (libConfig.person) {
          libConfig.person.id = null;
          libConfig.person.name = null;
        }
      }
      allPersons.value = reset
        ? page.persons
        : [...allPersons.value, ...page.persons];
      hasMorePersons.value = page.has_more;
      // `total` is the search-filtered visible count, so the header reflects the query.
      allPersonCount.value = page.total;
      if (allPersons.value.length > 0 && !selectedPerson.value && !selectedPersonWasFiltered) {
        const index = allPersons.value.findIndex(p => p.id === libConfig.person?.id);
        selectPerson(allPersons.value[index >= 0 ? index : 0]);
      }
    } else if (libConfig.person) {
      libConfig.person.id = null;
    }
  } finally {
    if (requestId === personLoadRequest) {
      isLoadingPersons.value = false;
      isLoadingMorePersons.value = false;
    }
  }
}

function handlePersonListScroll(event: Event) {
  const target = event.currentTarget as HTMLElement;
  if (target.scrollTop + target.clientHeight < target.scrollHeight - 24) return;
  void loadPersons(false);
}

function selectPerson(person: any) {
  if (isRenamingPerson.value) return;
  selectedPerson.value = person;
  if (!libConfig.person) {
    libConfig.person = { id: null, name: null };
  }
  libConfig.person.id = person.id;
  libConfig.person.name = person.name;
}

async function handleRenamePerson() {
  if (!isRenamingPerson.value) return;

  const newName = selectedPerson.value?.name?.trim() || '';

  if (newName.length === 0 || newName === originalPersonName.value) {
    isRenamingPerson.value = false;
    if (selectedPerson.value) {
      selectedPerson.value.name = originalPersonName.value;
    }
    return;
  }

  const result = await renamePerson(selectedPerson.value.id, newName);
  if (result) {
    isRenamingPerson.value = false;
    await loadPersons();
  }
}

function cancelRenamePerson() {
  if (selectedPerson.value) {
    selectedPerson.value.name = originalPersonName.value;
  }
  isRenamingPerson.value = false;
}

async function clickDeletePerson() {
  if (selectedPerson.value) {
    showDeletePersonMsgbox.value = false;
    const result = await deletePerson(selectedPerson.value.id);
    if (result) {
      const index = allPersons.value.findIndex(p => p.id === selectedPerson.value.id);
      allPersons.value = allPersons.value.filter(p => p.id !== selectedPerson.value.id);
      allPersonCount.value = Math.max(0, allPersonCount.value - 1);
      if (index > 0) {
        selectPerson(allPersons.value[index - 1]);
      } else if (index === 0) {
        if (allPersons.value.length > 0) {
          selectPerson(allPersons.value[0]);
        } else {
          selectedPerson.value = null;
          if (libConfig.person) {
            libConfig.person.id = null;
          }
        }
      } else {
        selectedPerson.value = null;
        if (libConfig.person) {
          libConfig.person.id = null;
        }
      }
    }
  }
}

// Called from title bar context menu
async function clickIndexFaces() {
  if (isIndexing.value) {
    return;
  }
  
  isIndexing.value = true;
  indexError.value = '';
  try {
    await indexFaces(config.faceClusterEpsilon);
  } catch (e) {
    indexError.value = String(e);
    isIndexing.value = false;
  }
}

// Cancel face indexing
async function clickCancelIndex() {
  await cancelFaceIndex();
}

// Reset faces
async function clickResetFaces() {
  showResetFacesMsgbox.value = true;
}

async function onResetFacesConfirm() {
  showResetFacesMsgbox.value = false;
  if (!await resetFaces()) {
    indexError.value = t('face_index.reset_failed');
    return;
  }
  
  // Reset selection and config
  selectedPerson.value = null;
  if (libConfig.person) {
    libConfig.person.id = null;
    libConfig.person.name = null;
  }

  if (personSearch.value) {
    personSearch.value = '';
    await nextTick();
    if (personSearchTimer) clearTimeout(personSearchTimer);
    personSearchTimer = null;
  }
  allPersonCount.value = 0;
  await loadPersons();
  checkFaceStats();
}

async function checkFaceStats() {
  const library = libConfig._libraryId;
  const stats = await getFaceStats();
  if (isPersonMounted && library === libConfig._libraryId && stats) {
    incompleteCount.value = stats.unprocessed;
  }
}

useLibraryChangeRefresh(() => {
  indexError.value = '';
  void restoreProgress();
  void loadPersons(true, true);
  void checkFaceStats();
});

// Only refresh the active view. Inactive panel data is refreshed on re-entry.

watch(() => [config.main.sidebarIndex, libConfig.activePane], () => {
  if (libConfig.activePane === 'main' && config.main.sidebarIndex === SIDEBAR.PERSON) {
    void loadPersons(true, true);
    void checkFaceStats();
  }
});

defineExpose({
  clickIndexFaces,
  clickCancelIndex,
  loadPersons,
  clickResetFaces,
  isIndexing,
});

</script>

<style scoped>
.face-status { display: flex; align-items: center; justify-content: space-between; gap: .5rem; margin: .25rem .5rem; padding: .5rem; border-radius: var(--radius-box); background: var(--color-base-200); font-size: .75rem; line-height: 1.45; overflow-wrap: anywhere; }
</style>
