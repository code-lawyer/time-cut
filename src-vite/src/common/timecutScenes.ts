import { invoke } from '@tauri-apps/api/core';

export const SCENES = ['travel', 'work', 'saved', 'daily', 'review'] as const;
export type Scene = typeof SCENES[number];
export type SceneFilter = Scene | 'pending';
export type SceneStatus = {
  library: string;
  counts: Partial<Record<SceneFilter, number>>;
  unavailable: number;
  failed: number;
  running: boolean;
  automatic: boolean;
  error: string | null;
};
export type FileScene = { automatic: Scene | null; manual: Scene | null };
export type SceneProvider = 'codex' | 'claude';

export const sceneStatus = (library: string) => invoke<SceneStatus>('timecut_scene_status', { library });
export const getFileScene = (library: string, fileId: number) => invoke<FileScene>('timecut_file_scene', { library, fileId });
export const setFileScene = (library: string, fileId: number, category: Scene | null) =>
  invoke('timecut_set_scene', { library, fileIds: [fileId], category });
export const getSceneProviders = () => invoke<SceneProvider[]>('timecut_scene_providers');
export const suggestScene = (library: string, fileId: number, provider: SceneProvider) =>
  invoke<Scene>('timecut_suggest_scene', { library, fileId, provider, sharePreview: true });
