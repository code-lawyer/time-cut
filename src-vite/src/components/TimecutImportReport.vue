<template>
  <section class="report" aria-labelledby="timecut-report-title">
    <div class="report-heading">
      <h2 id="timecut-report-title">{{ t(busy ? 'timecut.progress' : 'timecut.last') }}</h2>
      <span role="status" class="phase"><span v-if="busy" class="loading loading-spinner loading-xs"></span>{{ reportPhase(current) }}</span>
    </div>
    <p class="report-path" :title="current.source" dir="auto">{{ current.source }}</p>
    <p v-if="current.local_errors" class="issue-list" role="status">{{ t('timecut.localRepairCount', { count: n(current.local_errors) }) }} {{ current.local_error }}</p>
    <p v-else-if="current.safe_at" class="safe-note" role="status" data-testid="originals-safe">
      {{ t(liveRelease ? 'timecut.safeToUnplug' : receiptText) }} · {{ safeTime }}
      <span v-if="receiveDuration"> · {{ t('timecut.receiveDuration', { duration: receiveDuration }) }}</span>
    </p>
    <p v-if="current.capture && !current.safe_at" class="scan-counts">{{ t('timecut.cableHint') }}</p>
    <div v-if="current.list_total > 0" class="inventory-progress" data-testid="phone-list-progress">
      <p>{{ t('timecut.inventoryCoverage', { covered: n(current.list_covered || 0), total: n(current.list_total), remaining: n(Math.max(0, current.list_total - (current.list_covered || 0))) }) }}</p>
      <progress :max="current.list_total" :value="current.list_covered || 0" :aria-label="t('timecut.inventoryLabel')" />
      <p v-if="current.phase === '已核对本机接收记录' && current.newly_verified">{{ t('timecut.localProofCount', { count: n(current.newly_verified) }) }}</p>
      <p v-if="current.list_unseen">{{ t('timecut.unseenPending', { count: n(current.list_unseen) }) }}</p>
      <p v-if="current.source_mismatches">{{ t('timecut.sourceMismatchCount', { count: n(current.source_mismatches) }) }}</p>
      <p v-if="current.phase === '复核手机素材清单'">{{ t('timecut.confirmCount', { done: n(current.rechecked || 0), total: n(current.list_total) }) }}</p>
    </div>
    <div v-else-if="current.known_folders" class="inventory-progress" data-testid="phone-list-partial">
      <p>{{ t('timecut.partialInventory', { folders: n(current.listed_folders || 0), known: n(current.known_folders), items: n(current.listed_items || 0) }) }}</p>
      <p v-if="current.partial_coverage && current.listed_items">{{ t('timecut.partialCoverage', {
        matched: n(current.partial_coverage.matched), uncertain: n(current.partial_coverage.uncertain), missing: n(current.partial_coverage.missing)
      }) }}</p>
    </div>
    <p v-if="!current.safe_at && current.partial_coverage?.possible_jpeg_exports" class="scan-counts">
      {{ t('timecut.possibleJpegExports', { count: n(current.partial_coverage.possible_jpeg_exports) }) }}
    </p>
    <p v-if="!current.safe_at && current.partial_coverage?.possible_mov_exports" class="scan-counts">
      {{ t('timecut.possibleMovieExports', { count: n(current.partial_coverage.possible_mov_exports) }) }}
    </p>
    <p v-if="!current.safe_at && current.export_shortfall" class="scan-counts" role="status">
      {{ t('timecut.exportSpace', { required: formatFileSize(current.export_shortfall.required), available: formatFileSize(current.export_shortfall.available) }) }}
    </p>
    <div v-if="current.native_originals" class="inventory-progress" data-testid="apple-camera-progress">
      <p v-if="current.native_originals.repeat_verified">{{ t('timecut.appleCameraRepeat', { count: n(current.native_originals.listed) }) }}</p>
      <p v-else-if="current.native_originals.direct_attempts">{{ t('timecut.appleDirectChecks', { attempted: n(current.native_originals.direct_attempts), received: n(current.native_originals.direct_received) }) }}</p>
      <p v-else>{{ t('timecut.appleCameraCount', { checked: n(current.native_originals.matched), copied: n(current.native_originals.copied) }) }}</p>
      <p v-if="current.native_originals.discovered > current.native_originals.listed">{{ t('timecut.appleDiscovered', { count: n(current.native_originals.discovered) }) }}</p>
      <p v-if="current.native_originals.saved_receipts > current.native_originals.matched">{{ t('timecut.appleCameraSaved', { count: n(current.native_originals.saved_receipts) }) }}</p>
      <p v-if="current.native_originals.current_size">{{ t('timecut.transferKnown', {
        transferred: formatFileSize(current.native_originals.current_bytes), size: formatFileSize(current.native_originals.current_size)
      }) }}</p>
    </div>
    <p v-if="current.receive_attempt > 1" class="scan-counts">{{ t('timecut.cumulativeReceived', { attempt: n(current.receive_attempt), count: n(current.total_received), size: formatFileSize(current.total_received_bytes) }) }}</p>
    <p v-if="retryTime" class="scan-counts">{{ t('timecut.nextRetry', { time: retryTime }) }}</p>
    <p v-if="current.expected_minimum > current.discovered" class="scan-counts">
      {{ t('timecut.previousMedia', { count: n(current.expected_minimum) }) }}
    </p>
    <p v-if="busy || current.scanned" data-testid="import-scan-counts" class="scan-counts">
      {{ t('timecut.scanCounts', { scanned: n(current.scanned ?? 0), discovered: n(current.discovered ?? 0) }) }}
    </p>
    <p v-if="current.total_folders" class="scan-counts">{{ t('timecut.folderChecks', { checked: n(current.checked_folders || 0), total: n(current.total_folders) }) }}</p>
    <dl class="counts" data-testid="import-counts">
      <div v-for="key in counters" :key="key"><dt>{{ t('timecut.' + counterLabel(key)) }}</dt><dd>{{ n(current[key] || 0) }}</dd></div>
      <div v-if="current.pending > 0 && !(current.capture && current.list_total > 0)"><dt>{{ t('timecut.pendingMedia') }}</dt><dd>{{ n(current.pending) }}</dd></div>
    </dl>
    <p v-if="current.capture && current.received_bytes" class="scan-counts">{{ t('timecut.savedBytes', { size: formatFileSize(current.received_bytes) }) }}</p>
    <p v-if="current.capture && current.compared_bytes" class="scan-counts">{{ t('timecut.comparedBytes', { size: formatFileSize(current.compared_bytes) }) }}</p>
    <p v-if="current.safe_at && (current.local_pending || current.local_filed || current.local_duplicates)" class="scan-counts">
      {{ t('timecut.localCounts', { filed: n(current.local_filed || 0), duplicates: n(current.local_duplicates || 0), pending: n(current.local_pending || 0) }) }}
    </p>
    <div v-if="busy && current.current" class="transfer" data-testid="import-current-file">
      <p class="report-path" :title="current.current" dir="auto">{{ current.current }}</p>
      <span data-testid="import-transfer-bytes" class="transfer-size">{{ transferSize }}</span>
      <progress v-if="current.total_bytes > 0" class="transfer-progress" :aria-label="t('timecut.fileProgress')"
        :max="current.total_bytes" :value="Math.min(current.transferred_bytes, current.total_bytes)" />
    </div>
    <ul v-if="current.issues?.length" class="issue-list"><li v-for="(issue, i) in current.issues" :key="i">{{ issue }}</li></ul>
    <ul v-if="current.windows_export?.issues.length" class="issue-list">
      <li v-for="(issue, i) in current.windows_export.issues" :key="i">{{ issue }}</li>
    </ul>
    <details v-if="!busy && recent.length" class="history">
      <summary>{{ t('timecut.history') }}</summary>
      <ol><li v-for="(item, i) in recent" :key="i">
        <span class="history-name" dir="auto">{{ item.source }}</span>
        <span class="history-detail">{{ reportPhase(item) }} · {{ t(item.capture ? 'timecut.receiptHistory' : 'timecut.historyCounts', item) }}</span>
        <ul v-if="item.issues?.length" class="issue-list"><li v-for="(issue, j) in item.issues" :key="j">{{ issue }}</li></ul>
      </li></ol>
    </details>
  </section>
</template>

<script setup>
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { formatDuration, formatFileSize } from '@/common/utils';

const props = defineProps({ current: { type: Object, required: true }, busy: Boolean, recent: { type: Array, required: true }, retryAt: Number, liveRelease: Boolean });
const { t, n, locale } = useI18n();
const retryTime = computed(() => props.retryAt && new Date(props.retryAt * 1000).toLocaleTimeString(locale.value));
const counters = ['added', 'skipped', 'failed'];
const counterLabel = key => props.current.capture ? ({ added: 'received', skipped: 'alreadySaved' }[key] ?? key) : key;
const safeTime = computed(() => new Date(props.current.safe_at).toLocaleString(locale.value));
const receiptText = computed(() => ({
  apple_camera: 'timecut.appleOriginalsComplete',
  windows_export: 'timecut.windowsExportComplete',
}[props.current.phone_channel] ?? 'timecut.receiptComplete'));
const receiveDuration = computed(() => {
  const { capture_started_at: start, safe_at: end } = props.current;
  return Number.isSafeInteger(start) && Number.isSafeInteger(end) && end >= start
    ? formatDuration(Math.floor((end - start) / 1000)) : '';
});
const phases = {
  '正在盘点手机清单': 'listingPhone', '复核手机素材清单': 'confirmPhone',
  '核对手机旧版本': 'comparingSource', '确认手机返回的原件': 'confirmingStream',
  '核对手机目录': 'checkPhoneFolders', '核对本地收件': 'checkReceipts',
  '接收手机原件': 'receiving', '原件已收齐，可以拔线': 'receiptComplete',
  'Apple 相机原件已收齐': 'appleOriginalsComplete', 'Windows 导出已核对': 'windowsExportComplete',
  '正在本地整理': 'localFiling', '本地原件整理完成': 'localFiled',
  '已核对本机接收记录': 'localProofsChecked',
  '检查图库': 'checking', '正在读取': 'reading', '部分未完成': 'partial',
  '正在复制': 'copying', '设备已断开': 'disconnected', '已停止自动导入': 'stopped',
  '手机会话中断': 'sessionInterrupted',
  '手机返回内容与清单不符': 'sourceContentMismatch',
  '正在核对 Apple 相机原件': 'appleCameraScan', '已核对 Apple 相机原件': 'appleCameraChecked',
  'Apple 相机原件核对中断': 'appleCameraInterrupted',
  'Apple 相机原件待补': 'appleCameraPending',
  '等待 Apple 原件目录': 'appleCameraResume',
  '手机媒体暂不可读': 'mediaUnavailable', '仍有素材待补': 'resumePending',
  '等待完整媒体清单': 'incompleteInventory', '等待全量复扫': 'repeatQueued',
  '未发现可读照片或视频': 'empty', '已完成': 'complete',
};
const phase = value => phases[value] ? t('timecut.' + phases[value]) : value;
function reportPhase(report) {
  if (report.local_errors || report.phase === '本地收件待修复') return t('timecut.localRepair');
  if (report.windows_export?.busy) return t('timecut.windowsExportChecking', { count: n(report.windows_export.checked) });
  return report.verified ? t('timecut.repeatVerified') : phase(report.phase);
}
const transferSize = computed(() => {
  const { transferred_bytes = 0, total_bytes } = props.current;
  const transferred = formatFileSize(transferred_bytes);
  return total_bytes > 0
    ? t('timecut.transferKnown', { transferred, size: formatFileSize(total_bytes) })
    : t(['核对手机旧版本', '确认手机返回的原件'].includes(props.current.phase) ? 'timecut.comparingProgress' : 'timecut.transferUnknown', { transferred });
});
</script>

<style scoped>
.report { padding: 1rem 1.35rem; background: var(--color-base-200); color: var(--color-base-content); font-size: .8125rem; }
.report-heading, .phase, .counts, .counts > div { display: flex; align-items: center; }
.report-heading { justify-content: space-between; gap: .6rem; }
.report h2 { font-size: .98rem; font-weight: 650; }
.phase { gap: .4rem; }
.report-path { color: var(--soft-text); overflow: hidden; white-space: nowrap; text-overflow: ellipsis; margin-top: .3rem; user-select: text; }
.scan-counts { margin-top: .5rem; color: var(--soft-text); font-variant-numeric: tabular-nums; }
.safe-note { margin-top: .55rem; font-weight: 650; }
.inventory-progress { margin-top: .55rem; font-variant-numeric: tabular-nums; }
.inventory-progress progress { width: 100%; height: .45rem; accent-color: var(--color-primary); }
.counts { flex-wrap: wrap; gap: .4rem 1.5rem; margin-top: .5rem; font-variant-numeric: tabular-nums; }
.counts > div { gap: .35rem; }
.counts dt, .history-detail { color: var(--soft-text); }
.counts dd { font-weight: 650; }
.transfer { display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: baseline; gap: .35rem .65rem; margin-top: .4rem; }
.transfer .report-path { margin: 0; }
.transfer-size { font-variant-numeric: tabular-nums; white-space: nowrap; }
.transfer-progress { grid-column: 1 / -1; width: 100%; height: .45rem; accent-color: var(--color-primary); }
.history { margin-top: .5rem; }
.history summary { cursor: pointer; width: fit-content; padding-block: .25rem; }
.history ol > li { display: grid; gap: .15rem; border-top: 1px solid var(--line); padding: .5rem 0; }
.history-name, .issue-list { overflow-wrap: anywhere; user-select: text; }
.issue-list { margin-top: .3rem; color: color-mix(in srgb, var(--color-error) 59%, var(--color-base-content)); }
@media (max-width: 32rem), (max-height: 35rem) { .report { padding: .7rem .9rem; } }
</style>
