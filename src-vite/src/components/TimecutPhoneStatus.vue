<template>
  <section :class="['phone-status', { complete, warning }]" data-testid="phone-transfer-status">
    <component :is="icon" class="status-icon" aria-hidden="true" />
    <div class="status-copy">
      <h2 aria-live="polite">{{ t(heading) }}</h2>
      <p v-if="liveRelease && complete" class="unplug">{{ t('timecut.safeToUnplug') }}</p>
      <p v-else-if="complete && !noNew && !report.phone_channel" class="summary">{{ t('timecut.receiptComplete') }}</p>
      <p v-if="noNew" class="summary">{{ t(apple ? 'timecut.noNewAppleOriginals' : 'timecut.noNewOnPhone', { count: n(report.skipped) }) }}</p>
      <p v-else-if="complete && report.local_pending" class="summary">{{ t('timecut.filingContinues') }}</p>
      <p v-else-if="!complete && connected && !warning" class="summary">{{ t('timecut.cableHint') }}</p>
      <p v-if="!complete && report.previous_backup" class="summary">{{ t('timecut.knownPhoneBackup') }}</p>
      <p v-if="!noNew && (received || report.skipped)" class="summary numbers">
        {{ t('timecut.phoneTransferCounts', { received: n(received), skipped: n(report.skipped || 0) }) }}
      </p>
      <p v-if="apple && complete" class="summary" data-testid="windows-export-status">{{ exportStatus }}</p>
      <p v-if="apple && report.windows_export?.received" class="summary">
        {{ t('timecut.windowsExportSaved', { count: n(report.windows_export.received) }) }}
      </p>
      <p v-if="windows" class="summary">{{ t('timecut.windowsExportScope') }}</p>
    </div>
  </section>
</template>

<script setup>
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { IconChecked, IconInformation, IconUpload } from '@/common/icons';

const props = defineProps({
  report: { type: Object, required: true },
  busy: Boolean,
  connected: Boolean,
  historical: Boolean,
  liveRelease: Boolean,
});
const { t, n } = useI18n();
const complete = computed(() => !!props.report.safe_at && !props.report.local_errors);
const apple = computed(() => props.report.phone_channel === 'apple_camera');
const windows = computed(() => props.report.phone_channel === 'windows_export');
const warning = computed(() => !!props.report.local_errors || !!props.report.failed);
const received = computed(() => Math.max(props.report.total_received || 0, props.report.added || 0));
const noNew = computed(() => complete.value && !received.value && props.report.skipped > 0);
const icon = computed(() => complete.value ? IconChecked : warning.value ? IconInformation : IconUpload);
const exportStatus = computed(() => {
  const status = props.report.windows_export;
  if (status?.complete) return t('timecut.windowsExportVerified', { count: n(status.checked) });
  if (status?.busy) return t('timecut.windowsExportChecking', { count: n(status.checked) });
  if (status?.listed != null) {
    const remaining = Math.max(0, status.listed - status.checked);
    return t(remaining > 0 ? 'timecut.windowsExportPending' : 'timecut.windowsExportConfirming', {
      checked: n(status.checked), total: n(status.listed), remaining: n(remaining),
    });
  }
  return t('timecut.windowsExportUnconfirmed', { count: n(status?.checked ?? 0) });
});
const heading = computed(() => {
  if (props.report.local_errors) return 'timecut.localRepair';
  if (complete.value && apple.value) return 'timecut.appleOriginalsComplete';
  if (complete.value && windows.value) return 'timecut.windowsExportComplete';
  if (complete.value) return props.historical ? 'timecut.lastPhoneTransferComplete' : 'timecut.phoneTransferComplete';
  if (props.historical) return 'timecut.lastPhoneTransferIncomplete';
  if (!props.connected) return 'timecut.phoneDisconnected';
  if (props.report.failed) return 'timecut.phoneTransferIncomplete';
  if (props.busy) {
    return props.report.phase === '接收手机原件' ? 'timecut.receiving'
      : props.report.previous_backup ? 'timecut.checkingPhoneBackup' : 'timecut.checkingPhoneMedia';
  }
  return received.value ? 'timecut.finalizingPhoneTransfer' : 'timecut.mediaUnavailable';
});
</script>

<style scoped>
.phone-status {
  --status-accent: var(--color-primary);
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  flex-shrink: 0;
  align-items: start;
  gap: .75rem;
  margin: .9rem 1.35rem .1rem;
  padding: .85rem 1rem;
  border: 1px solid color-mix(in srgb, var(--status-accent) 54%, var(--color-base-100));
  border-radius: .75rem;
  color: var(--color-base-content);
  background: color-mix(in srgb, var(--status-accent) 10%, var(--color-base-100));
}
.phone-status.complete { --status-accent: var(--color-success); }
.phone-status.warning { --status-accent: var(--color-warning); }
.status-icon {
  width: 1.25rem;
  height: 1.25rem;
  margin-top: .12rem;
  color: color-mix(in srgb, var(--status-accent) 55%, var(--color-base-content));
}
.status-copy h2 { font-size: 1.05rem; line-height: 1.35; font-weight: 700; }
.unplug { margin-top: .16rem; font-weight: 650; }
.summary { margin-top: .18rem; font-size: .8125rem; color: var(--soft-text); }
.numbers { font-variant-numeric: tabular-nums; }
@media (max-width: 32rem), (max-height: 35rem) {
  .phone-status { margin: .65rem .9rem .1rem; padding: .6rem .7rem; gap: .55rem; }
}
</style>
