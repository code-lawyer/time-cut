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
            <a :href="projectRepository + '/issues'" target="_blank" rel="noopener noreferrer"
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
      <p class="px-1 text-sm">{{ $t('timecut.updateNotice') }}</p>
      <a :href="projectRepository + '/releases'" target="_blank" rel="noopener noreferrer"
        class="inline-flex items-center gap-1.5 rounded-box px-2 py-1 text-sm hover:text-primary">
        <IconGithub class="t-icon-size-sm" /> {{ $t('timecut.releaseDownloads') }}
      </a>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref, onMounted } from 'vue';
import { getPackageInfo, getBuildTime } from '@/common/api';
import { IconGithub, IconFocus } from '@/common/icons';
import privacy from '../../../PRIVACY.md?raw';
import iconLogo from '@/assets/images/icon.png';

const projectRepository = (import.meta.env.VITE_TIMECUT_REPOSITORY_URL || 'https://github.com/code-lawyer/time-cut').replace(/\/+$/, '');
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
