import { readFileSync } from 'node:fs';

export function readTestSource(path) {
  return readFileSync(new URL(path, import.meta.url), 'utf8').replace(/\r\n?/g, '\n');
}
