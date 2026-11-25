// Run against an unpacked, packaged exe. Only generated QA directories are used.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { DatabaseSync } from 'node:sqlite';

const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const executable = path.resolve(process.argv[2]);
const ffmpeg = path.join(path.dirname(executable), 'ffmpeg', 'ffmpeg-x86_64-pc-windows-msvc.exe');
const working = path.join(project, '.timecut-build');
fs.mkdirSync(working, { recursive: true });
const qa = fs.mkdtempSync(path.join(working, 'desktop-qa-'));
const makeDir = (name) => {
  const folder = path.join(qa, name);
  fs.mkdirSync(folder, { recursive: true });
  return folder;
};
const archive = makeDir('原件图库'),
  source = makeDir('来源甲'),
  second = makeDir('来源乙');
const linked = makeDir('原位索引'),
  data = makeDir('用户数据'),
  webData = makeDir('WebView');
const tempResult = { qa, executable, checks: {} };
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const cmd = (name, args, env = process.env) => {
  const r = spawnSync(name, args, { encoding: 'utf8', windowsHide: true, env, timeout: 120000 });
  if (r.status !== 0) throw Error(String(r.error || r.stderr || r.stdout || 'exit ' + r.status));
  return r.stdout.trim();
};
const photo = path.join(project, 'src-tauri', 'icons', 'icon.png');
const picture = path.join(source, '相机', 'IMG_007.png');
fs.mkdirSync(path.dirname(picture), { recursive: true });
fs.copyFileSync(photo, picture);
const repeated = path.join(source, '重复素材', '另一个名字.png');
fs.mkdirSync(path.dirname(repeated), { recursive: true });
fs.copyFileSync(photo, repeated);
fs.copyFileSync(photo, path.join(second, '从手机保存.png'));
const other = path.join(source, '下载', 'IMG_007.png');
fs.mkdirSync(path.dirname(other), { recursive: true });
cmd(ffmpeg, [
  '-v',
  'error',
  '-f',
  'lavfi',
  '-i',
  'color=c=cyan:s=96x64',
  '-frames:v',
  '1',
  '-threads',
  '1',
  other,
]);
fs.copyFileSync(other, path.join(linked, '留在原处.png'));
const sameDay = new Date(2021, 5, 4, 12);
fs.utimesSync(picture, sameDay, sameDay);
fs.utimesSync(other, sameDay, sameDay);
const video = path.join(source, '短片.mp4');
cmd(ffmpeg, [
  '-v',
  'error',
  '-f',
  'lavfi',
  '-i',
  'color=c=blue:s=128x96:d=0.7',
  '-c:v',
  'libx264',
  '-pix_fmt',
  'yuv420p',
  '-metadata',
  'creation_time=2019-03-16T10:20:30Z',
  video,
]);
const heic = path.join(source, '原始图片.heic');
fs.copyFileSync(
  path.join(project, 'src-tauri', 'third_party', 'libheif', 'examples', 'example.heic'),
  heic,
);
const desired = [photo, other, video, heic];
if (process.argv[3] && process.argv[4]) {
  for (const supplied of process.argv.slice(3, 5)) {
    const ext = path.extname(supplied);
    const dest = path.join(source, '实况样本' + ext);
    fs.copyFileSync(supplied, dest);
    desired.push(dest);
  }
}
const expected = desired.map((p) => hash(fs.readFileSync(p))).sort();
const files = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    if (e.name === '.time-cut') return [];
    const p = path.join(dir, e.name);
    return e.isDirectory() ? files(p) : [p];
  });
const freePort = await new Promise((resolve) => {
  const s = net.createServer();
  s.listen(0, '127.0.0.1', () => {
    const port = s.address().port;
    s.close(() => resolve(port));
  });
});
const env = {
  ...process.env,
  TIMECUT_DATA_DIR: data,
  WEBVIEW2_USER_DATA_FOLDER: webData,
  WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS:
    '--remote-debugging-port=' + freePort + ' --remote-debugging-address=127.0.0.1',
};
// Confirm the portable runtime and codecs suffice even when the developer tools are off PATH.
const pathKey = Object.keys(env).find((key) => key.toLowerCase() === 'path') || 'Path';
env[pathKey] = [path.join(env.SystemRoot, 'System32'), env.SystemRoot].join(path.delimiter);
const app = spawn(executable, [], { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
const stdout = fs.createWriteStream(path.join(qa, 'app.log'));
app.stdout.pipe(stdout, { end: false });
app.stderr.pipe(stdout, { end: false });
let socket,
  nextId = 0;
const calls = new Map(),
  events = [];
function send(method, params = {}) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    calls.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
async function evaluate(expression) {
  const out = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (out.exceptionDetails)
    throw Error(out.exceptionDetails.exception?.description ?? out.exceptionDetails.exception?.value ?? out.exceptionDetails.text);
  return out.result.value;
}
async function waitFor(test, label, ms = 90000) {
  const end = Date.now() + ms;
  let last = '';
  while (Date.now() < end) {
    if (app.exitCode !== null) throw Error('Process exited ' + app.exitCode + ' while ' + label);
    try {
      const value = await test();
      if (value) return value;
    } catch (e) {
      last = String(e);
    }
    await delay(350);
  }
  throw Error('Timed out: ' + label + ' ' + last);
}
const inspect = async (command, args = {}) => {
  const reply = await inPage(async ({ command, args }) => {
    try { return { ok: true, value: await window.__TAURI_INTERNALS__.invoke(command, args) }; }
    catch (error) { return { ok: false, message: String(error) }; }
  }, { command, args });
  if (!reply.ok) throw Error(reply.message);
  return reply.value;
};
const inPage = (fn, arg) => evaluate('(' + fn.toString() + ')(' + JSON.stringify(arg ?? null) + ')');
const saveShot = async (name, clip) => {
  const shot = await send('Page.captureScreenshot', { format: 'png', ...(clip && { clip }) });
  fs.writeFileSync(path.join(qa, name), Buffer.from(shot.data, 'base64'));
};
async function keyPress(key, code, modifiers = 0) {
  for (const type of ['keyDown', 'keyUp']) {
    await send('Input.dispatchKeyEvent', { type, key, code: key, windowsVirtualKeyCode: code, modifiers });
  }
}
async function checkDialogLayout() {
  const previousTheme = await evaluate("document.documentElement.getAttribute('data-theme')");
  const samples = [];
  try {
    for (const layout of [
      { width: 1200, height: 800, theme: 'dark' },
      { width: 800, height: 600, theme: 'light' },
      { width: 480, height: 480, theme: 'dark' },
    ]) {
      await send('Emulation.setDeviceMetricsOverride', { width: layout.width, height: layout.height, deviceScaleFactor: 1, mobile: false });
      await inPage(theme => document.documentElement.setAttribute('data-theme', theme), layout.theme);
      await inPage(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      const sample = await inPage(() => {
        const panel = document.querySelector('[data-testid=import-dialog]');
        const r = panel.getBoundingClientRect();
        const scroller = panel.querySelector('.import-scroll');
        const canvas = document.createElement('canvas');
        canvas.width = canvas.height = 1;
        const ctx = canvas.getContext('2d', { willReadFrequently: true });
        const luminance = css => {
          ctx.fillStyle = css;
          ctx.fillRect(0, 0, 1, 1);
          const [r, g, b] = [...ctx.getImageData(0, 0, 1, 1).data].map(raw => {
            const c = raw / 255;
            return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
          });
          return 0.2126 * r + 0.7152 * g + 0.0722 * b;
        };
        const values = [getComputedStyle(panel.querySelector('.help')).color, getComputedStyle(panel).backgroundColor].map(luminance);
        return {
          rect: [r.left, r.top, r.right, r.bottom], viewport: [innerWidth, innerHeight],
          contentWidth: scroller.scrollWidth, availableWidth: scroller.clientWidth,
          hintContrast: (Math.max(...values) + .05) / (Math.min(...values) + .05),
        };
      });
      const [left, top, right, bottom] = sample.rect;
      assert.ok(left >= 0 && top >= 0 && right <= layout.width + 1 && bottom <= layout.height + 1, JSON.stringify(sample));
      assert.ok(sample.contentWidth <= sample.availableWidth + 1, 'horizontal overflow ' + JSON.stringify(sample));
      assert.ok(sample.hintContrast >= 4.5, 'hint contrast ' + sample.hintContrast);
      samples.push({ ...layout, ...sample });
      if (layout.width !== 1200) await saveShot('import-' + layout.theme + '-' + layout.width + '.png');
    }
  } finally {
    await send('Emulation.clearDeviceMetricsOverride');
    await inPage(theme => theme ? document.documentElement.setAttribute('data-theme', theme) : document.documentElement.removeAttribute('data-theme'), previousTheme);
  }
  await inPage(() => document.querySelector('[data-testid=import-close]').focus());
  await keyPress('Tab', 9, 8); // Shift+Tab wraps from the first control to the last.
  const wrapped = await evaluate("document.querySelector('[data-testid=import-dialog]').contains(document.activeElement) && !document.activeElement.matches('[data-testid=import-close]')");
  assert.equal(wrapped, true, 'focus remains inside the dialog');
  await keyPress('Tab', 9);
  assert.equal(await evaluate("document.activeElement.matches('[data-testid=import-close]')"), true);
  await keyPress('Escape', 27);
  await waitFor(() => evaluate("!document.querySelector('[data-testid=import-dialog]') && document.activeElement.matches('[data-testid=import-open]')"), 'Escape restores the opener');
  await click('import-open');
  return { samples, escapeRestoresFocus: true, tabWraps: true };
}
async function click(id) {
  const selector = JSON.stringify('[data-testid="' + id + '"]:not(:disabled)');
  await waitFor(
    () => evaluate('!!document.querySelector(' + selector + ')'),
    'enabled control ' + id,
  );
  await evaluate('document.querySelector(' + selector + ').click()');
}
const fill = (id, value) =>
  evaluate(
    '(()=>{const e=document.getElementById(' +
      JSON.stringify(id) +
      ');e.value=' +
      JSON.stringify(value) +
      ';e.dispatchEvent(new Event("input",{bubbles:true}));})()',
  );
async function folder(value, indexOnly = false) {
  await fill('timecut-source', value);
  await evaluate(
    '(()=>{const s=document.querySelector("[data-testid=import-mode]");s.value=' +
      JSON.stringify(indexOnly ? 'index' : 'copy') +
      ';s.dispatchEvent(new Event("change",{bubbles:true}));})()',
  );
  await click('folder-start');
}

async function openSidebar(index) {
  const labels = {
    6: ['人物', 'People'], 7: ['地点', '地點', 'Locations'], 9: ['内容分类', '內容分類', 'Categories'],
  }[index] ?? [];
  const selector = [
    `[data-sidebar="${index}"] [role=button]:not([aria-disabled=true])`,
    ...labels.map(label => `[role=button][aria-label="${label}"]:not([aria-disabled=true])`),
  ].join(',');
  await waitFor(() => inPage(s => !!document.querySelector(s), selector), 'available sidebar ' + index);
  await inPage(s => document.querySelector(s).click(), selector);
}

async function verifyOrganizer(library, fixtures) {
  const input = makeDir('整理样本');
  const followUp = makeDir('整理增量样本');
  const originalPortrait = path.join(fixtures, 'lena.jpg');
  const rotatedPortrait = path.join(input, 'portrait-a.jpg');
  cmd(ffmpeg, ['-v', 'error', '-i', originalPortrait,
    '-vf', 'crop=480:450:16:30,scale=416:390,transpose=2', '-frames:v', '1', rotatedPortrait]);
  const rotatedBytes = fs.readFileSync(rotatedPortrait);
  // The stored pixels are shifted 90 degrees counterclockwise. EXIF 6 displays them upright.
  const orientation = Buffer.from('ffe1002245786966000049492a0008000000010012010300010000000600000000000000', 'hex');
  fs.writeFileSync(rotatedPortrait, Buffer.concat([rotatedBytes.subarray(0, 2), orientation, rotatedBytes.subarray(2)]));
  fs.copyFileSync(path.join(fixtures, 'messi5.jpg'), path.join(input, 'portrait-b.jpg'));
  fs.copyFileSync(heic, path.join(input, 'cold-location.heic'));
  const variant = path.join(followUp, 'portrait-a-resized.jpg');
  fs.copyFileSync(originalPortrait, variant);
  for (const work of [true, false]) {
    const png = await inPage(work => {
      const canvas = document.createElement('canvas');
      canvas.width = 864;
      canvas.height = 648;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#f4f7fb';
      ctx.fillRect(0, 0, 864, 648);
      ctx.fillStyle = work ? '#263851' : '#47408d';
      ctx.fillRect(0, 0, 864, 78);
      ctx.fillStyle = 'white';
      ctx.font = 'bold 32px Arial';
      ctx.fillText(work ? 'Quarterly sales report' : 'https://example.org / news feed', 40, 49);
      ctx.fillStyle = '#28405e';
      ctx.font = 'bold 26px Arial';
      ctx.fillText(work ? 'Department revenue and expenses' : 'Your city today', 40, 138);
      for (let i = 0; i < 8; i++) {
        const y = 170 + 48 * i;
        ctx.fillStyle = i % 2 ? '#dfe9f5' : 'white';
        ctx.fillRect(40, y, 784, 42);
        ctx.fillStyle = '#28405e';
        ctx.font = '23px Arial';
        ctx.fillText(work ? 'Cost center ' + (10 + i) + '                Actual     Budget     Variance' :
          'Read article   ·   Share   ·   Sign in   ·   Recommended', 56, y + 29);
      }
      return canvas.toDataURL('image/png').split(',')[1];
    }, work);
    fs.writeFileSync(path.join(input, work ? 'work-report.png' : 'Screenshot test webpage.png'), Buffer.from(png, 'base64'));
  }

  const datadir = path.join(data, 'libraries');
  const catalog = new DatabaseSync(path.join(datadir, library + '.db'), { readOnly: true, timeout: 3000 });
  const q = catalog.prepare(
    'SELECT a.id,a.name,s.category,s.manual_category,a.has_faces,a.geo_name,a.taken_date FROM afiles a LEFT JOIN timecut_scenes s ON a.id=s.file_id WHERE a.name=?',
  );
  const identities = catalog.prepare('SELECT DISTINCT person_id FROM faces JOIN afiles ON file_id=afiles.id WHERE name=? AND person_id IS NOT NULL');
  try {
    await openSidebar(9);
    await click('scene-filter-saved');
    await inspect('timecut_folder', { path: input, indexOnly: true });
    const classified = await waitFor(
      () => ['cold-location.heic', 'work-report.png', 'Screenshot test webpage.png']
        .map(name => q.get(name)).filter(row => row?.category).length === 3,
      'local classification of unopened HEIC and PNG files', 120000,
    );
    assert.ok(classified);
    assert.equal(q.get('cold-location.heic').category, 'travel');
    assert.equal(q.get('work-report.png').category, 'work');
    const gps = process.argv[3] && q.get('实况样本' + path.extname(process.argv[3]));
    if (gps) assert.ok(gps.geo_name && gps.taken_date > 0);
    const location = gps && (await inspect('get_location_info', { sort: 0 }))
      .find(item => item.names.includes(gps.geo_name));
    if (gps) assert.ok(location);
    const identity = await waitFor(() => identities.get('portrait-a.jpg')?.person_id, 'local model creates the first person', 120000);
    const different = await waitFor(() => identities.get('portrait-b.jpg')?.person_id, 'local model finds the second person', 30000);
    assert.notEqual(different, identity);
    assert.ok(await inspect('get_person_thumbnail', { personId: identity }));
    const geometry = catalog.prepare('SELECT width,height,e_orientation FROM afiles WHERE name=?').get('portrait-a.jpg');
    assert.equal(geometry.e_orientation, 6);
    assert.ok(geometry.width > geometry.height, 'Catalog dimensions follow the display orientation');
    await inspect('rename_person', { personId: identity, name: '样本人物甲' });
    await inspect('timecut_folder', { path: followUp, indexOnly: true });
    await waitFor(() => identities.get(path.basename(variant))?.person_id, 'automatic grouping on a later import', 130000);
    assert.equal(identities.get(path.basename(variant)).person_id, identity);
    assert.equal(catalog.prepare('SELECT name FROM persons WHERE id=?').get(identity).name, '样本人物甲');
    const differentPath = path.join(input, 'portrait-b.jpg');
    cmd(ffmpeg, ['-v', 'error', '-y', '-f', 'lavfi', '-i', 'color=c=white:s=352x240',
      '-frames:v', '1', differentPath]);
    await inspect('timecut_folder', { path: input, indexOnly: true });
    await waitFor(() => !identities.get('portrait-b.jpg'), 'old people assignments are cleared when a photo is replaced');

    const sample = q.get('Screenshot test webpage.png');
    const found = await inspect('timecut_file_scene', { library, fileId: sample.id });
    await inspect('timecut_set_scene', { library, fileIds: [sample.id], category: 'travel' });
    assert.deepEqual(await inspect('timecut_file_scene', { library, fileId: sample.id }), {
      automatic: found.automatic, manual: 'travel',
    });
    await click('scene-filter-travel');
    await waitFor(() => evaluate("document.querySelector('[data-testid=scene-filter-travel]').getAttribute('aria-pressed') === 'true' && document.querySelectorAll('[id^=item-] img').length > 0"), 'category album lists the selected items');
    const providerCheck = await inspect('timecut_scene_providers');
    assert.ok(Array.isArray(providerCheck));
    await assert.rejects(inspect('timecut_suggest_scene', {
      library, fileId: sample.id, provider: 'codex', sharePreview: false,
    }), /preview/i);
    await saveShot('categories-dark.png');
    await send('Emulation.setDeviceMetricsOverride', { width: 800, height: 600, deviceScaleFactor: 1, mobile: false });
    await evaluate("document.documentElement.setAttribute('data-theme','light')");
    await delay(300);
    const infoVisible = await inPage(() => {
      const rect = document.querySelector('[data-testid=gallery-info]').getBoundingClientRect();
      return rect.width > 0 && rect.left >= 0 && rect.right <= innerWidth;
    });
    assert.ok(infoVisible, 'The detail button remains reachable in a narrow window');
    await saveShot('categories-light-800.png');
    await inPage(() => document.querySelector('[data-testid=gallery-info] [role=button]').click());
    await waitFor(() => evaluate("!!document.querySelector('[data-testid=scene-file-category]:enabled')"), 'category editing in the photo information panel');
    await click('scene-agent-open');
    await waitFor(() => evaluate("document.querySelector('[data-testid=scene-agent-open]').parentElement.open"), 'AI tools are an explicit action');
    await saveShot('category-details-light-800.png');
    await inPage(() => document.querySelector('[data-testid=gallery-info] [role=button]').click());
    await openSidebar(6);
    await waitFor(() => evaluate("document.body.innerText.includes('样本人物甲')"), 'visible named person');
    await saveShot('people-light-800.png');
    if (location) {
      await openSidebar(7);
      await waitFor(() => inPage(region => document.body.innerText.includes(region), location.admin1), 'visible place album');
      await saveShot('locations-light-800.png');
    }
    await send('Emulation.clearDeviceMetricsOverride');
    await evaluate("document.documentElement.setAttribute('data-theme','dark')");
    return {
      namesSurviveIncrementalGrouping: true,
      differentPeopleSeparated: true,
      peopleOnRotatedJpeg: true,
      changedOriginalClearsOldFace: true,
      heicProcessed: await waitFor(() => q.get('cold-location.heic')?.has_faces > 0, 'face analysis processed HEIC', 90000),
      categories: ['cold-location.heic', 'work-report.png', 'Screenshot test webpage.png'].map(name => ({ name, automatic: q.get(name).category })),
      location: gps?.geo_name ?? null,
      manualOverride: true,
      aiPreviewRequiresConsent: true,
      detailsReachableAt800px: true,
    };
  } finally {
    catalog.close();
  }
}
let failure;
try {
  let tab = await waitFor(async () => {
    const r = await fetch('http://127.0.0.1:' + freePort + '/json/list');
    return (await r.json()).find(
      (t) => t.type === 'page' && t.webSocketDebuggerUrl && /tauri|localhost/i.test(t.url),
    );
  }, 'packaged WebView2 target');
  socket = new WebSocket(tab.webSocketDebuggerUrl);
  await new Promise((ok, fail) => {
    socket.onopen = ok;
    socket.onerror = fail;
  });
  socket.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id) {
      const h = calls.get(m.id);
      if (h) {
        calls.delete(m.id);
        m.error ? h.reject(Error(m.error.message)) : h.resolve(m.result);
      }
    } else if (m.method === 'Runtime.exceptionThrown')
      events.push(
        m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text,
      );
  };
  await send('Runtime.enable');
  await send('Page.enable');
  await waitFor(
    () => evaluate('!!document.querySelector("[data-testid=import-open]")'),
    'import entry',
  );
  const version = await inspect('plugin:app|version');
  const expectedVersion = JSON.parse(
    fs.readFileSync(path.join(project, 'src-tauri', 'tauri.conf.json'), 'utf8'),
  ).version;
  assert.equal(version, expectedVersion);
  tempResult.checks.version = version;
  if (!(await evaluate('!!document.getElementById("timecut-root")'))) await click('import-open');
  await waitFor(() => evaluate('!!document.getElementById("timecut-root")'), 'import dialog');
  tempResult.checks.accessibility = await checkDialogLayout();
  await fill('timecut-root', path.join(qa, '尚未创建的目录'));
  await click('target-save');
  await waitFor(() => evaluate("!!document.querySelector('[data-testid=import-dialog] [role=alert]')"), 'visible destination error');
  assert.equal((await inspect('timecut_status')).target, null);
  tempResult.checks.invalidDestinationCanBeCorrected = true;
  await fill('timecut-root', archive);
  // The test never imports a personal phone that happens to be attached.
  await evaluate(
    '(()=>{const c=document.querySelector("[role=dialog] input[type=checkbox]");if(c.checked)c.click()})()',
  );
  await click('target-save');
  await waitFor(async () => {
    const s = await inspect('timecut_status');
    return s.target?.root === archive && !s.target.auto;
  }, 'save local destination');
  await assert.rejects(inspect('timecut_folder', { path: path.join(archive, '.time-cut', 'staging'), indexOnly: true }), /内部目录/);
  tempResult.checks.internalFolderRejectedForIndex = true;
  await click('target-edit');
  await fill('timecut-root', path.join(qa, '尚未保存的图库'));
  await fill('timecut-source', source);
  assert.equal(await evaluate("document.querySelector('[data-testid=folder-start]').disabled"), true);
  await inPage(() => document.querySelector('form [type=button].quiet-button').click());
  tempResult.checks.unsavedDestinationBlocksCopy = true;
  await folder(source);
  const first = await waitFor(async () => {
    const s = await inspect('timecut_status');
    return s.recent.find((r) => r.source === source);
  }, 'first copy');
  assert.deepEqual([first.added, first.skipped, first.failed], [expected.length, 1, 0]);
  tempResult.checks.first = first;
  await folder(second);
  const duplicate = await waitFor(async () => {
    const s = await inspect('timecut_status');
    return s.recent.find((r) => r.source === second);
  }, 'cross-source duplicate');
  assert.deepEqual([duplicate.added, duplicate.skipped, duplicate.failed], [0, 1, 0]);
  tempResult.checks.duplicate = duplicate;
  await folder(source);
  const repeat = await waitFor(async () => {
    const s = await inspect('timecut_status');
    return s.recent.find((r) => r.source === source && r.added === 0);
  }, 'repeat first source');
  assert.deepEqual([repeat.added, repeat.skipped, repeat.failed], [0, expected.length + 1, 0]);
  tempResult.checks.repeat = repeat;
  assert.deepEqual(
    files(archive)
      .map((f) => hash(fs.readFileSync(f)))
      .sort(),
    expected,
  );
  const collision = files(archive).filter((file) => path.basename(file).startsWith('IMG_007'));
  assert.equal(collision.length, 2);
  assert.equal(path.dirname(collision[0]), path.dirname(collision[1]));
  tempResult.checks.collision = collision.map((file) => path.basename(file)).sort();
  const archivedVideo = files(archive).find((file) => path.basename(file) === '短片.mp4');
  assert.equal(path.basename(path.dirname(archivedVideo)), '2019-03');
  tempResult.checks.videoYearMonth = path.basename(path.dirname(archivedVideo));
  const target = (await inspect('timecut_status')).target;
  const minimumVisible = expected.length - (process.argv[4] ? 1 : 0);
  await waitFor(
    async () => Number((await inspect('get_album_visible_counts'))[target.album]) >= minimumVisible,
    'originals visible in Lap',
    180000,
  );
  const dates = await inspect('get_taken_dates', { sort: 0 });
  assert.ok(dates.some(([date, count]) => date.startsWith('2019-03-16') && count > 0));
  tempResult.checks.timelineIncludesVideoCaptureDate = true;
  if (process.argv[4]) {
    const databases = fs
      .readdirSync(data, { recursive: true })
      .filter((p) => path.basename(p) === target.library + '.db');
    assert.equal(databases.length, 1);
    const catalog = new DatabaseSync(path.join(data, databases[0]), { readOnly: true });
    try {
      const q = catalog.prepare(
        "SELECT p.name AS still, m.name AS movie FROM afiles AS p JOIN afiles AS m ON m.id=p.live_photo_video_id WHERE p.media_subtype='live_photo'",
      );
      const pair = await waitFor(
        () => q.all().find((r) => r.still.startsWith('实况样本') && r.movie.startsWith('实况样本')),
        'Live Photo pair',
      );
      tempResult.checks.livePhoto = pair;
      const indexedHeic = catalog
        .prepare("SELECT name,width,height FROM afiles WHERE name LIKE '%.heic'")
        .get();
      assert.ok(indexedHeic && indexedHeic.width > 0 && indexedHeic.height > 0);
      tempResult.checks.heic = indexedHeic;
      tempResult.checks.geotaggedFiles = catalog
        .prepare(
          'SELECT count(*) AS n FROM afiles WHERE gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL AND (gps_latitude<>0 OR gps_longitude<>0)',
        )
        .get().n;
    } finally {
      catalog.close();
    }
  }
  await folder(linked, true);
  const inPlace = await waitFor(
    async () => {
      const albums = await inspect('get_all_albums', { refreshAccessibility: false });
      const a = albums.find(
        (a) =>
          path.toNamespacedPath(a.path).toLowerCase() ===
          path.toNamespacedPath(linked).toLowerCase(),
      );
      if (!a) return false;
      return Number((await inspect('get_album_visible_counts'))[a.id]) >= 1 && a;
    },
    'in-place photos visible',
    180000,
  );
  assert.equal(files(archive).length, expected.length);
  tempResult.checks.indexOnly = { id: inPlace.id, visible: true };
  const db = new DatabaseSync(path.join(archive, '.time-cut', 'imports.sqlite3'), {
    readOnly: true,
  });
  tempResult.checks.archive = {
    files: files(archive).length,
    sourceRows: db.prepare('SELECT count(*) AS n FROM sources').get().n,
    sha256: expected,
  };
  db.close();
  await saveShot('desktop.png');
  const clip = await inPage(() => {
    const { x, y, width, height } = document.querySelector('[data-testid=import-dialog]').getBoundingClientRect();
    return { x, y, width, height, scale: 1 };
  });
  await saveShot('import-panel.png', clip);
  await click('import-close');
  await waitFor(
    () => evaluate('!document.querySelector("[data-testid=import-dialog]")'),
    'close import panel',
  );
  const visibleGrid = await waitFor(
    () =>
      evaluate(
        'document.querySelectorAll("[id^=item-] img").length >= 2 && document.querySelectorAll("[id^=item-] img").length',
      ),
    'album thumbnails on the main screen',
    25000,
  );
  tempResult.checks.visibleThumbnails = visibleGrid;
  const gallery = await send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(path.join(qa, 'gallery.png'), Buffer.from(gallery.data, 'base64'));
  await openSidebar(9);
  await click('scene-filter-all');
  await send('Emulation.setDeviceMetricsOverride', { width: 800, height: 600, deviceScaleFactor: 1, mobile: false });
  await inPage(() => document.querySelector('[data-testid=gallery-info] [role=button]').click());
  const categoryWidth = await waitFor(() => inPage(() => {
    const input = document.querySelector('[data-testid=scene-file-category]:enabled');
    return input && input.getBoundingClientRect().width;
  }), 'visible category selector in compact details');
  assert.ok(categoryWidth >= 90, 'The category selector uses the width of the compact details panel');
  await saveShot('compact-details.png');
  await inPage(() => document.querySelector('[data-testid=gallery-info] [role=button]').click());
  await send('Emulation.clearDeviceMetricsOverride');
  await openSidebar(1);
  tempResult.checks.compactDetailsWidth = categoryWidth;
  tempResult.checks.deviceProbe = JSON.parse(cmd(executable, ['--probe-devices'], env));
  assert.equal(await inspect('timecut_tray_active'), true);
  await inspect('plugin:window|close', { label: 'main' });
  await waitFor(
    async () => !(await inspect('plugin:window|is_visible', { label: 'main' })),
    'close to tray',
    15000,
  );
  // Exercise the same native import service with the main window hidden.
  await inspect('timecut_folder', { path: linked, indexOnly: false });
  const background = await waitFor(
    async () => (await inspect('timecut_status')).recent.find((r) => r.source === linked),
    'background copy',
  );
  assert.deepEqual([background.added, background.skipped, background.failed], [0, 1, 0]);
  const reopen = spawn(executable, [], { env, windowsHide: true, stdio: 'ignore' });
  try {
    await waitFor(
      async () =>
        reopen.exitCode === 0 && (await inspect('plugin:window|is_visible', { label: 'main' })),
      'restore original window on second launch',
      15000,
    );
  } finally {
    if (reopen.exitCode === null) reopen.kill();
  }
  tempResult.checks.tray = { hiddenImport: true, restoredOnSecondLaunch: true };
  if (process.env.TIMECUT_QA_ORGANIZER_FIXTURES) {
    tempResult.checks.organizer = await verifyOrganizer(target.library, process.env.TIMECUT_QA_ORGANIZER_FIXTURES);
  }
  // Exercise catalog actions through the packaged application's native commands.
  // All catalog IDs and paths in this section belong to this run's isolated profile.
  const additional = await inspect('add_library', { name: '第二个测试图库' });
  assert.ok(additional.id && additional.id !== target.library);
  await inspect('switch_library', { id: additional.id });
  await inspect('timecut_set_auto', { auto: false });
  assert.equal((await inspect('timecut_status')).target.library, target.library);
  await assert.rejects(inspect('index_album', {
    albumId: target.album, thumbnailSize: 512, rawDisplayOptions: {}, groupRawJpegPairs: true, expectedLibrary: target.library,
  }), /Library changed/);
  await inspect('switch_library', { id: target.library });

  const newRoot = path.join(qa, '改名后的图库');
  assert.equal(path.dirname(fs.realpathSync(archive)), fs.realpathSync(qa));
  assert.equal(path.dirname(newRoot), qa);
  assert.equal(fs.existsSync(newRoot), false);
  assert.ok(await inspect('rename_folder', { folderPath: archive, newFolderName: path.basename(newRoot) }));
  await waitFor(async () => (await inspect('timecut_status')).target?.root === newRoot, 'import tracks renamed album');
  assert.equal(fs.existsSync(archive), false);
  const following = makeDir('改名后重新导入');
  fs.copyFileSync(picture, path.join(following, '同一张照片.png'));
  await inspect('timecut_folder', { path: following, indexOnly: false });
  const afterRename = await waitFor(async () => (await inspect('timecut_status')).recent.find(row => row.source === following), 'copy to renamed album');
  assert.deepEqual([afterRename.added, afterRename.skipped, afterRename.failed], [0, 1, 0]);
  assert.ok(await inspect('remove_album', { id: target.album }));
  await waitFor(async () => !(await inspect('timecut_status')).target, 'removed album releases import destination');
  await assert.rejects(inspect('timecut_folder', { path: following, indexOnly: false }), /请选择|请先选定|已移除/);
  assert.deepEqual(files(newRoot).map(file => hash(fs.readFileSync(file))).sort(), expected);
  tempResult.checks.catalogLifecycle = {
    autoTogglePreservedOwner: true, staleScanRejected: true, renamedDestinationResumes: true,
    removedDestinationRejected: true, originalHashesIntact: expected.length,
  };
  tempResult.browserExceptions = events;
  assert.deepEqual(events, []);
} catch (e) {
  failure = e;
  tempResult.error = String(e.stack || e);
  if (socket?.readyState === WebSocket.OPEN) {
    try {
      tempResult.diagnostic = {
        text: await evaluate('document.body.innerText'),
        status: await inspect('timecut_status'),
        events,
      };
      const shot = await send('Page.captureScreenshot', { format: 'png' });
      fs.writeFileSync(path.join(qa, 'desktop.png'), Buffer.from(shot.data, 'base64'));
    } catch {}
  }
} finally {
  socket?.close();
  if (app.exitCode === null) app.kill();
  stdout.end();
  fs.writeFileSync(path.join(qa, 'result.json'), JSON.stringify(tempResult, null, 2));
  console.log(path.join(qa, 'result.json'));
  if (failure) {
    console.error(failure);
    process.exitCode = 1;
  }
}
