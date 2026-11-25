import fs from 'node:fs';
import path from 'node:path';

export function saveQaResult(project, directory, report, failed) {
  const result = path.join(directory, 'result.json');
  fs.writeFileSync(result, JSON.stringify(report, null, 2));
  console.log(result);
  if (failed || process.env.TIMECUT_QA_KEEP_FIXTURES === '1') return;

  const root = path.join(fs.realpathSync(project), '.timecut-build');
  const resolved = fs.realpathSync(directory);
  if (path.dirname(resolved) !== root || !/^(desktop|recovery)-qa-[a-zA-Z0-9]{6}$/.test(path.basename(resolved))) {
    throw new Error('QA cleanup expected a generated directory inside the project build directory');
  }
  const targets = fs.readdirSync(resolved)
    .filter(name => name !== 'result.json')
    .map(name => path.join(resolved, name));
  for (const target of targets) {
    if (fs.realpathSync(target) !== target) throw new Error(`QA cleanup encountered a redirected path ${target}`);
  }
  for (const target of targets) fs.rmSync(target, { recursive: true, force: true, maxRetries: 8 });
}
