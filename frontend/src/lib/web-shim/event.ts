/**
 * Drop-in replacement for `@tauri-apps/api/event` in the self-hosted web build.
 *
 * Server-side events arrive over SSE (`GET /api/events`) and are fanned out to the
 * same `listen()` callbacks the desktop app uses. Events the browser produces itself
 * (recording lifecycle, `model-config-updated`) go through the same bus, so UI
 * components cannot tell the difference.
 */

export type UnlistenFn = () => void;

export interface Event<T> {
  event: string;
  id: number;
  payload: T;
}

export type EventCallback<T> = (event: Event<T>) => void;

/** Subset of Tauri's built-in event names the app references. */
export const TauriEvent = {
  DRAG_DROP: 'tauri://drag-drop',
  DRAG_ENTER: 'tauri://drag-enter',
  DRAG_LEAVE: 'tauri://drag-leave',
  WINDOW_CLOSE_REQUESTED: 'tauri://close-requested',
} as const;

const listeners = new Map<string, Set<EventCallback<unknown>>>();
let nextEventId = 1;
let source: EventSource | null = null;

function dispatch(name: string, payload: unknown): void {
  const handlers = listeners.get(name);
  if (!handlers?.size) return;
  const event: Event<unknown> = { event: name, id: nextEventId++, payload };
  // Copy first: a handler may unlisten itself.
  for (const handler of Array.from(handlers)) {
    try {
      handler(event);
    } catch (error) {
      console.error(`[web-shim] listener for "${name}" threw`, error);
    }
  }
}

function connect(): void {
  if (source || typeof window === 'undefined') return;

  source = new EventSource('/api/events', { withCredentials: true });

  source.onmessage = (message) => {
    try {
      const frame = JSON.parse(message.data) as { event?: string; payload?: unknown };
      if (frame?.event) dispatch(frame.event, frame.payload ?? null);
    } catch (error) {
      console.error('[web-shim] malformed event frame', message.data, error);
    }
  };

  // EventSource reconnects on its own; a permanent failure usually means the session
  // expired, and the next invoke() bounces the user to /login.
  source.onerror = () => {
    if (source?.readyState === EventSource.CLOSED) {
      console.warn('[web-shim] event stream closed, will retry on next listen()');
      source = null;
    }
  };
}

export async function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  connect();
  let handlers = listeners.get(event);
  if (!handlers) {
    handlers = new Set();
    listeners.set(event, handlers);
  }
  handlers.add(handler as EventCallback<unknown>);
  return () => {
    handlers?.delete(handler as EventCallback<unknown>);
    if (handlers && handlers.size === 0) listeners.delete(event);
  };
}

export async function once<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  const unlisten = await listen<T>(event, (payload) => {
    unlisten();
    handler(payload);
  });
  return unlisten;
}

/** Frontend-originated event. There is no other window to notify, so stay local. */
export async function emit(event: string, payload?: unknown): Promise<void> {
  dispatch(event, payload ?? null);
}

export async function emitTo(_target: string, event: string, payload?: unknown): Promise<void> {
  dispatch(event, payload ?? null);
}

/** Used by the shim itself to synthesize events the server cannot know about. */
export function emitLocal(event: string, payload?: unknown): void {
  dispatch(event, payload ?? null);
}
