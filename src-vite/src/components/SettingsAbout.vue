<template>
  <div class="flex flex-col items-start justify-start gap-4 h-full text-base-content/70 cursor-default">

    <!-- logo -->
    <div class="px-2 flex w-full flex-row items-center justify-start gap-4">
      <div class="shrink-0">
        <img :src="iconLogo" class="w-20 h-20 select-none [-webkit-app-region:no-drag]" draggable="false" />
      </div>
      <div class="flex flex-col text-left">
        <h3 class="text-xl">Time-cut</h3>
        <p class="mt-2 text-sm">{{ $t('timecut.aboutDescription') }}</p>
      </div>
    </div>

    <!-- package info -->
    <div class="w-full rounded-box border border-base-content/5 bg-base-300/30 p-4 shadow-sm">
      <div class="space-y-3 text-left">
        <div class="grid grid-cols-[84px_1fr] items-start gap-3 text-sm">
          <div class="text-base-content/30">
            {{ $t('settings.about.package.version') }}
          </div>
          <div>{{ displayVersion }}</div>
        </div>

        <div class="grid grid-cols-[84px_1fr] items-start gap-3 text-sm">
          <div class="text-base-content/30">
            {{ $t('settings.about.package.build_time') }}
          </div>
          <div>{{ buildTime }}</div>
        </div>

        <div class="grid grid-cols-[84px_1fr] items-start gap-3 text-sm">
          <div class="text-base-content/30">
            {{ $t('settings.about.package.license') }}
          </div>
          <div>{{ packageInfo.license }}</div>
        </div>

        <div class="grid grid-cols-[84px_1fr] items-center gap-1 text-sm">
          <div class="text-base-content/30">
            {{ $t('settings.about.package.link') }}
          </div>
          <div class="flex flex-wrap items-center justify-start">
            <a href="https://github.com/julyx10/lap/tree/v0.4.1" target="_blank" rel="noopener noreferrer"
              class="inline-flex items-center gap-1.5 rounded-box px-2 py-1 text-xs hover:text-primary">
              <IconGithub class="t-icon-size-sm" /> Lap v0.4.1
            </a>
            <a v-if="projectRepository" :href="projectRepository + '/issues'" target="_blank" rel="noopener noreferrer"
              class="inline-flex items-center gap-1.5 rounded-box px-2 py-1 text-xs hover:text-primary">
              <IconFocus class="t-icon-size-sm" /> {{ $t('settings.about.package.feedback') }}
            </a>
          </div>
        </div>
      </div>
    </div>

    <details class="w-full rounded-box bg-base-300/30 p-3 text-sm">
      <summary class="cursor-pointer">{{ $t('settings.about.package.privacy') }}</summary>
      <p class="mt-2">{{ $t('timecut.privacySummary') }}</p>
      <div v-for="(part, i) in privacyParagraphs" :key="i" class="mt-2">
        <strong v-if="part.startsWith('#')">{{ part.replace(/^#+\s*/, '') }}</strong>
        <p v-else>{{ part }}</p>
      </div>
    </details>

    <!-- updates -->
    <div class="w-full rounded-box p-2 space-y-2 bg-base-300/30 border border-base-content/5 shadow-sm">
      <div class="flex items-center gap-2 text-base-content/30">
        <span class="font-bold uppercase text-[10px] tracking-widest">{{ $t('settings.about.section_updates') }}</span>
      </div>
      <p v-if="!updaterEnabled" class="px-1 text-sm">{{ $t('timecut.updateNotice') }}</p>
      <div v-else class="flex items-center justify-between gap-4 px-1 rounded-box hover:bg-base-100/10 transition-colors duration-200">
        <div class="min-w-0 flex-1 flex flex-col gap-0.5 text-sm leading-5">
          <div>{{ updateStatusText }}</div>
          <progress
            v-if="isDownloadingUpdate"
            class="progress progress-primary mt-1 h-1.5 w-full"
            :value="downloadPercent === null ? undefined : downloadPercent"
            max="100"
          ></progress>
        </div>
        <button
          class="btn btn-sm btn-ghost rounded-box bg-base-100 border border-base-content/30 text-base-content/70 hover:text-base-content shrink-0"
          :disabled="isCheckingUpdate || isInstallingUpdate || isDownloadingUpdate"
          @click="handleUpdateAction"
        >
          <span v-if="isCheckingUpdate || isInstallingUpdate || isDownloadingUpdate" class="loading loading-spinner loading-xs"></span>
          <span v-else>{{ updateButtonText }}</span>
        </button>
      </div>
      <div v-if="updaterEnabled" class="flex items-center justify-between gap-4 px-1 rounded-box hover:bg-base-100/10 transition-colors duration-200">
        <div class="flex flex-col gap-0.5 text-sm leading-5">
          <div>{{ $t('settings.general.auto_check_updates') }}</div>
        </div>
        <input type="checkbox" class="toggle toggle-primary toggle-sm" v-model="config.settings.autoCheckUpdates" />
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref, onMounted } from 'vue';
import { useI18n } from 'vue-i18n';
import { getPackageInfo, getBuildTime } from '@/common/api';
import { useAppUpdater, updaterEnabled, projectRepository } from '@/common/updater';
import { config } from '@/common/config';
import { IconGithub, IconFocus } from '@/common/icons';
import privacy from '../../../PRIVACY.md?raw';
import iconLogo from '@/assets/images/icon.png';

const packageInfo = ref<any>({
  name: '',
  description: '',
  version: '',
  commit_hash: '',
  license: '',
  authors: [],
  homepage: '',
  repository: ''
});
const buildTime = ref('');
const displayVersion = computed(() => {
  const version = packageInfo.value.version || '';
  const commitHash = packageInfo.value.commit_hash || packageInfo.value.commitHash || '';
  return commitHash ? `${version} (${commitHash})` : version;
});
const privacyParagraphs = privacy.trim().split(/\r?\n\s*\r?\n/);
const { locale, messages } = useI18n();
const localeMsg = computed(() => messages.value[locale.value] as any);
const {
  isCheckingUpdate,
  isInstallingUpdate,
  isDownloadingUpdate,
  isUpdateReadyToRestart,
  updateAvailable,
  updateVersion,
  downloadProgressLabel,
  downloadPercent,
  checkError,
  updateButtonText,
  handleUpdateAction,
} = useAppUpdater(localeMsg, { toastPlacement: 'center', silent: true });

// Status line for the Updates section: reflects the current update state.
const updateStatusText = computed(() => {
  const au = localeMsg.value.settings.about.auto_update;
  if (isDownloadingUpdate.value) return downloadProgressLabel.value;
  if (isInstallingUpdate.value) return au.installing;
  if (isCheckingUpdate.value) return au.checking;
  if (checkError.value) return checkError.value;
  if (isUpdateReadyToRestart.value) return au.update_installed_waiting_restart;
  if (updateAvailable.value && updateVersion.value) {
    return au.new_version_available.replace('{version}', updateVersion.value);
  }
  return au.latest_version;
});

onMounted(async () => {
  try {
    packageInfo.value = await getPackageInfo();
    const time = await getBuildTime();
    buildTime.value = time || '';
  } catch (error) {
    console.error('Failed to load about info:', error);
  }
});
</script>
