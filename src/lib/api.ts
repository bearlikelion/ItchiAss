import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Status = "none" | "downloaded" | "update";

export interface Settings {
  logged_in: boolean;
  download_dir: string;
  parallel: number;
  extract: boolean;
  cleanup: boolean;
  by_creator: boolean;
  check_on_launch: boolean;
}

export interface UploadFile {
  id: number;
  filename: string;
  display_name: string | null;
  size: number | null;
  downloaded: boolean;
  skipped: boolean;
}

export interface Game {
  game_id: number;
  key_id: number;
  title: string;
  creator: string;
  classification: string;
  cover_url: string | null;
  url: string | null;
  platforms: string[];
  status: Status;
}

export interface Progress {
  game_id: number;
  upload_id: number;
  filename: string;
  received: number;
  total: number;
  state: "queued" | "downloading" | "extracting" | "done" | "error" | "cancelled";
  error: string | null;
}

export const getSettings = () => invoke<Settings>("get_settings");
export const saveSettings = ({ logged_in: _, ...input }: Settings) => invoke<void>("save_settings", { input });
export const login = () => invoke<void>("login");
export const setApiKey = (key: string) => invoke<void>("set_api_key", { key });
export const logout = () => invoke<void>("logout");
export const cachedLibrary = () => invoke<Game[]>("cached_library");
export const library = () => invoke<Game[]>("library");
export const uploads = (g: Game) => invoke<UploadFile[]>("uploads", { gameId: g.game_id, keyId: g.key_id });
export const setSkipped = (uploadId: number, skipped: boolean) => invoke<void>("set_skipped", { uploadId, skipped });
export const cancel = (uploadIds: number[]) => invoke<void>("cancel", { uploadIds });
export const openFolder = (gameId: number) => invoke<void>("open_folder", { gameId });
export const checkUpdates = (games: Game[]) =>
  invoke<Record<string, Status>>("check_updates", {
    items: games.map((g) => [g.game_id, g.key_id]),
  });
export const download = (games: Game[]) =>
  invoke<void>("download", {
    items: games.map((g) => ({
      game_id: g.game_id,
      key_id: g.key_id,
      creator: g.creator,
      title: g.title,
    })),
  });
export const onProgress = (cb: (p: Progress) => void): Promise<UnlistenFn> =>
  listen<Progress>("download-progress", (e) => cb(e.payload));
