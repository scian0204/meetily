/**
 * Replacements for the Tauri plugins the UI imports but a browser cannot use.
 *
 * One module backs several aliases (`plugin-store`, `plugin-os`, `plugin-updater`,
 * `plugin-process`, `api/app`, `api/path`) — the export names do not collide, and a
 * single file keeps the webpack alias table short.
 */

// ---------------------------------------------------------------------------
// @tauri-apps/plugin-store -> localStorage
// ---------------------------------------------------------------------------

/** Same surface as the plugin's `Store`, backed by one localStorage key per file. */
export class Store {
  private readonly storageKey: string;
  private cache: Record<string, unknown>;

  constructor(path: string) {
    this.storageKey = `meetily_store:${path}`;
    this.cache = this.read();
  }

  private read(): Record<string, unknown> {
    if (typeof window === 'undefined') return {};
    try {
      const raw = window.localStorage.getItem(this.storageKey);
      return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
    } catch {
      return {};
    }
  }

  private write(): void {
    if (typeof window === 'undefined') return;
    try {
      window.localStorage.setItem(this.storageKey, JSON.stringify(this.cache));
    } catch (error) {
      console.warn(`[web-shim] could not persist ${this.storageKey}`, error);
    }
  }

  async get<T>(key: string): Promise<T | null> {
    const value = this.cache[key];
    return value === undefined ? null : (value as T);
  }

  async set(key: string, value: unknown): Promise<void> {
    this.cache[key] = value;
    this.write();
  }

  async delete(key: string): Promise<boolean> {
    const existed = key in this.cache;
    delete this.cache[key];
    this.write();
    return existed;
  }

  async has(key: string): Promise<boolean> {
    return key in this.cache;
  }

  async keys(): Promise<string[]> {
    return Object.keys(this.cache);
  }

  async values<T>(): Promise<T[]> {
    return Object.values(this.cache) as T[];
  }

  async entries<T>(): Promise<[string, T][]> {
    return Object.entries(this.cache) as [string, T][];
  }

  async length(): Promise<number> {
    return Object.keys(this.cache).length;
  }

  async clear(): Promise<void> {
    this.cache = {};
    this.write();
  }

  async reset(): Promise<void> {
    await this.clear();
  }

  /** Writes are synchronous here; kept for API compatibility. */
  async save(): Promise<void> {
    this.write();
  }

  async reload(): Promise<void> {
    this.cache = this.read();
  }

  async close(): Promise<void> {
    /* nothing to release */
  }

  async onKeyChange(): Promise<() => void> {
    return () => undefined;
  }

  async onChange(): Promise<() => void> {
    return () => undefined;
  }
}

/** The plugin's lazy variant behaves identically once constructed. */
export class LazyStore extends Store {}

const stores = new Map<string, Store>();

export async function load(path: string): Promise<Store> {
  let store = stores.get(path);
  if (!store) {
    store = new Store(path);
    stores.set(path, store);
  }
  return store;
}

export const getStore = load;

// ---------------------------------------------------------------------------
// @tauri-apps/plugin-os
// ---------------------------------------------------------------------------

export type Platform = 'linux' | 'macos' | 'windows' | 'android' | 'ios';

export function platform(): Platform {
  if (typeof navigator === 'undefined') return 'linux';
  const agent = navigator.userAgent;
  if (/Android/i.test(agent)) return 'android';
  if (/iPhone|iPad|iPod/i.test(agent)) return 'ios';
  if (/Mac OS X|Macintosh/i.test(agent)) return 'macos';
  if (/Windows/i.test(agent)) return 'windows';
  return 'linux';
}

export function version(): string {
  return 'web';
}

export function type(): Platform {
  return platform();
}

export function arch(): string {
  return 'unknown';
}

// ---------------------------------------------------------------------------
// @tauri-apps/plugin-updater and plugin-process
// ---------------------------------------------------------------------------

export interface Update {
  version: string;
  currentVersion: string;
  date?: string;
  body?: string;
  downloadAndInstall: (onEvent?: (event: unknown) => void) => Promise<void>;
  close: () => Promise<void>;
}

/** The server image updates with `docker compose pull`, so there is nothing to check. */
export async function check(): Promise<Update | null> {
  return null;
}

export async function relaunch(): Promise<void> {
  if (typeof window !== 'undefined') window.location.reload();
}

export async function exit(_code?: number): Promise<void> {
  /* a browser tab does not exit itself */
}

// ---------------------------------------------------------------------------
// @tauri-apps/api/app
// ---------------------------------------------------------------------------

export async function getVersion(): Promise<string> {
  return process.env.NEXT_PUBLIC_APP_VERSION ?? '0.4.0';
}

export async function getName(): Promise<string> {
  return 'Meetily';
}

export async function getTauriVersion(): Promise<string> {
  return 'web';
}

// ---------------------------------------------------------------------------
// @tauri-apps/api/path
// ---------------------------------------------------------------------------
// Paths only reach display strings and server-side filenames in the web build, so
// virtual values are enough. Real storage paths live on the server.

export async function appDataDir(): Promise<string> {
  return '/data';
}

export async function appLocalDataDir(): Promise<string> {
  return '/data';
}

export async function appConfigDir(): Promise<string> {
  return '/data';
}

export async function downloadDir(): Promise<string> {
  return '/data/recordings';
}

export async function join(...parts: string[]): Promise<string> {
  return parts
    .filter(Boolean)
    .join('/')
    .replace(/\/{2,}/g, '/');
}
