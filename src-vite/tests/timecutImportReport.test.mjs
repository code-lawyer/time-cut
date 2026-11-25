import assert from 'node:assert/strict';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';
import { createSSRApp, h } from 'vue';
import { createI18n } from 'vue-i18n';
import { renderToString } from 'vue/server-renderer';

const root = fileURLToPath(new URL('../', import.meta.url));
const messages = { 'zh-CN': JSON.parse(fs.readFileSync(new URL('../src/locales/zh-CN.json', import.meta.url), 'utf8')) };
const started = Date.UTC(2026, 0, 1);
let server, Report;

before(async () => {
  const memory = new Map();
  globalThis.window = { navigator: globalThis.navigator, matchMedia: () => ({ matches: false }) };
  globalThis.localStorage = {
    getItem: key => memory.get(key) ?? null, setItem: (key, value) => memory.set(key, value),
    removeItem: key => memory.delete(key),
  };
  server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'error' });
  Report = (await server.ssrLoadModule('/src/components/TimecutImportReport.vue')).default;
});
after(async () => { await server?.close(); });

async function render(state, busy = false) {
  const current = { capture: true, phase: '接收手机原件', capture_started_at: started, source: 'iPhone', ...state };
  const app = createSSRApp({ render: () => h(Report, { current, busy, recent: [] }) });
  app.use(createI18n({ legacy: false, locale: 'zh-CN', messages }));
  return renderToString(app);
}

test('a directory outage reports direct lookups and distinguishes them from a complete camera list', async () => {
  const native = { listed: 1, matched: 1, copied: 1, saved_receipts: 2551, direct_attempts: 2, direct_received: 1 };
  const html = await render({ phase: 'Apple 相机原件核对中断', native_originals: native });
  assert.match(html, /按已知路径补查 2 项，已确认 1 项/);
  assert.match(html, /2,551/);
  assert.doesNotMatch(html, /Apple 相机目录已核对 1 项/);
});

test('folder enumeration and verified file counts stay visible independently', async () => {
  const native = { discovered: 2551, listed: 1, matched: 1, copied: 0, saved_receipts: 2550 };
  const html = await render({ phase: '正在核对 Apple 相机原件', native_originals: native }, true);
  assert.match(html, /本次相机目录已列出 2,551 项/);
  assert.match(html, /Apple 相机目录已核对 1 项/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});

test('partial directories retain a separate count for existing, uncertain and missing originals', async () => {
  const html = await render({
    listed_folders: 2, known_folders: 4, listed_items: 18,
    partial_coverage: { matched: 9, uncertain: 6, missing: 3 },
    receive_attempt: 2, total_received: 1, total_received_bytes: 1_250_000_000,
  });
  assert.match(html, /已保存其中 2 个目录的清单，共列出 18 项/);
  assert.match(html, /9 项能匹配已备份版本，6 项需比对内容，3 项尚未匹配到备份/);
  assert.match(html, /1\.25 GB/);
  assert.doesNotMatch(html, /data-testid="phone-list-progress"|data-testid="originals-safe"/);
});

test('a complete list and matching receipts still require final device and local checks', async () => {
  const waiting = await render({ list_total: 7, list_covered: 5, list_unseen: 2, source_mismatches: 1, pending: 13_509 });
  assert.match(waiting, /共 7 项，5 项已有备份凭据，2 项待核对/);
  assert.match(waiting, /另有 2 条历史待补记录/);
  assert.match(waiting, /1 项曾返回与清单不符的内容/);
  assert.doesNotMatch(waiting, /13,509/);
  assert.doesNotMatch(waiting, /data-testid="originals-safe"/);
  const confirming = await render({ phase: '复核手机素材清单', list_total: 7, list_covered: 7, rechecked: 3 });
  assert.match(confirming, /最后复核已对上 3 \/ 7 项/);
  assert.doesNotMatch(confirming, /data-testid="originals-safe"/);
});

test('a JPEG export candidate stays pending in the full source count', async () => {
  const html = await render({ list_total: 4, list_covered: 1, partial_coverage: { possible_jpeg_exports: 2 } });
  assert.match(html, /2 个待核对的 JPEG/);
  assert.match(html, /4 项，1 项已有备份凭据，3 项待核对/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});

test('only a recorded release displays the cable notice and measured duration', async () => {
  const old = await render({ capture_started_at: null, safe_at: started });
  assert.match(old, /data-testid="originals-safe"/);
  assert.doesNotMatch(old, /本轮接收用时/);
  const released = await render({ safe_at: started + 7_215_871 });
  assert.match(released, /本轮接收用时 2:00:15/);
});

test('JPEG export storage limits show required and available space and keep originals pending', async () => {
  const html = await render({ list_total: 4, list_covered: 1, partial_coverage: { possible_jpeg_exports: 2, possible_mov_exports: 1 },
    export_shortfall: { required: 35_000_000_000, available: 23_000_000_000 } });
  assert.match(html, /35(?:\.0+)? GB/);
  assert.match(html, /23(?:\.0+)? GB/);
  assert.match(html, /暂缓/);
  assert.match(html, /1 个 MOV/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});

test('an outstanding local filing error supersedes an earlier release and repeat', async () => {
  const html = await render({ local_errors: 1, local_error: '示例原件校验失败', safe_at: started, verified: true });
  assert.match(html, /有 1 项本地收件尚未归档/);
  assert.match(html, /示例原件校验失败/);
  assert.doesNotMatch(html, /data-testid="originals-safe"|全量复扫完成/);
});

test('offline receipts show the saved count without an unplug notice', async () => {
  const html = await render({ phase: '已核对本机接收记录', list_total: 12, list_covered: 7, newly_verified: 3 });
  assert.match(html, /从现有备份确认了 3 项/);
  assert.match(html, /共 12 项，7 项已有备份凭据，5 项待核对/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});

test('a completed Apple camera check keeps the Windows inventory and unplug notice independent', async () => {
  const html = await render({ phase: '已核对 Apple 相机原件', list_total: 10, list_covered: 8,
    native_originals: { matched: 12, copied: 2, current_size: null } });
  assert.match(html, /Apple 相机目录已核对 12 项，本次补收 2 份原件/);
  assert.match(html, /共 10 项，8 项已有备份凭据，2 项待核对/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});

test('an independently repeated Apple inventory reports its scope while Windows receipts remain open', async () => {
  const report = { phase: '已核对 Apple 相机原件', list_total: 10, list_covered: 8,
    native_originals: { listed: 12, matched: 12, copied: 0, stable_inventory: true, repeat_verified: true } };
  const html = await render(report);
  assert.match(html, /Apple 原件目录复扫 12 项，新增 0 项，缺失 0 项/);
  assert.match(html, /2 项待核对/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
  report.native_originals.repeat_verified = false;
  assert.doesNotMatch(await render(report), /Apple 原件目录复扫 12 项/);
  report.native_originals = { listed: 0, matched: 0, copied: 0, saved_receipts: 2550 };
  assert.match(await render(report), /本地累计保存了 2,550 项 Apple 原件接收记录/);
});

test('a recovery read identifies the content check and keeps the cable notice hidden', async () => {
  const html = await render({ phase: '确认手机返回的原件', current: 'sample.jpg',
    transferred_bytes: 128, total_bytes: null }, true);
  assert.match(html, /正在验证手机返回的文件内容/);
  assert.match(html, /已核对 128 B/);
  assert.doesNotMatch(html, /data-testid="originals-safe"/);
});
