<template>

  <!-- Custom Title Bar -->
  <div 
    :class="[
      'w-full flex items-center justify-between select-none cursor-default',
      viewName==='ImageViewer' ? 'h-12' : 'h-10',
    ]"
    @contextmenu.prevent
    data-tauri-drag-region
  >
    <!-- Title Name -->
    <!-- Icon & Title Container -->
    <div v-if="isMac" class="flex-1" data-tauri-drag-region></div>
    <div 
      :class="[
        'flex items-center overflow-hidden',
        showDesktopWindowControls ? 'ml-2' : '',
        isMac ? 'justify-center text-center' : ''
      ]"
      data-tauri-drag-region
    >
      <!-- Icon -->
      <img 
        v-if="icon" 
        :src="icon" alt=""
        class="w-5 h-5 mr-2 select-none rounded" 
        data-tauri-drag-region 
      />
      
      <!-- Title Name -->
      <span 
        class="text-nowrap text-base-content/70 overflow-hidden whitespace-pre text-ellipsis"
        data-tauri-drag-region
      >
        {{ titlebar }}
      </span>
    </div>
    <div v-if="isMac" class="flex-1" data-tauri-drag-region></div>

    <!-- Center Slot -->
    <div
      :class="[
        isMac ? 'hidden' : 'flex-1 flex items-center justify-center'
      ]"
      data-tauri-drag-region
    >
      <slot></slot>
    </div>

    <!-- Window Control Buttons -->
    <div v-if="showDesktopWindowControls" class="h-10 mb-auto flex items-center" @mousedown.stop>
      <button v-if="resizable" type="button" class="window-control"
        :aria-label="$t('timecut.minimizeWindow')" @click.stop="appWindow.minimize()">
        <IconWinMinus aria-hidden="true" />
      </button>
      <button v-if="resizable" type="button" class="window-control"
        :aria-label="$t(isMaximized ? 'timecut.restoreWindow' : 'timecut.maximizeWindow')" @click.stop="toggleMaximizeWindow">
        <component :is="isMaximized ? IconWinRestore : IconWinMaximize" aria-hidden="true" />
      </button>
      <button type="button" class="window-control close-window"
        :aria-label="$t('timecut.closeWindow')" @click.stop="appWindow.close()">
        <IconClose aria-hidden="true" />
      </button>
    </div>

  </div>

</template>

<script setup>

import { onMounted, onUnmounted, ref } from 'vue';
import { getCurrentWindow  } from '@tauri-apps/api/window';
import { isWin, isMac, isLinux } from '@/common/utils';

import { 
  IconWinMinus,
  IconWinMaximize,
  IconWinRestore,
  IconClose 
} from '@/common/icons';

const props = defineProps({
  titlebar: {
    type: String,
    required: true,
  },
  viewName: {
    type: String,
    required: false,
  },
  resizable: {
    type: Boolean,
    default: true,
  },
  icon: {
    type: String,
    default: '',
  }
});

const appWindow = getCurrentWindow();
const isMaximized = ref(false);
const showDesktopWindowControls = isWin || isLinux;
let stopResize;
let disposed = false;

async function syncWindowState() { isMaximized.value = await appWindow.isMaximized(); }
async function toggleMaximizeWindow() {
  if (await appWindow.isMaximized()) await appWindow.unmaximize();
  else await appWindow.maximize();
  await syncWindowState();
}
onMounted(async () => {
  if (!showDesktopWindowControls || !props.resizable) return;
  const stop = await appWindow.onResized(syncWindowState);
  if (disposed) { stop(); return; }
  stopResize = stop;
  await syncWindowState();
});
onUnmounted(() => { disposed = true; stopResize?.(); });
</script>

<style scoped>
.window-control { display: grid; place-items: center; width: 3rem; height: 100%; color: var(--color-base-content); }
.window-control svg { width: 1rem; height: 1rem; }
.window-control:hover { background: var(--color-base-100); }
.window-control:focus-visible { outline: 2px solid var(--color-primary); outline-offset: -3px; }
.close-window:hover { background: #c42b1c; color: white; }
</style>
