import type { DeckState } from "./events";

const DATABASE_NAME = "muteki-run-projections";
const DATABASE_VERSION = 1;
// Version 5 replays older snapshots to populate per-worker prompt history.
const SNAPSHOT_VERSION = 5;
const STORE_NAME = "snapshots";
const MAX_SNAPSHOTS = 6;

export interface RunProjectionSnapshot {
  runId: string;
  seq: number;
  deck: DeckState;
  savedAt: number;
  version: number;
}

let databasePromise: Promise<IDBDatabase | null> | null = null;

function openDatabase(): Promise<IDBDatabase | null> {
  if (typeof indexedDB === "undefined") return Promise.resolve(null);
  if (databasePromise) return databasePromise;
  databasePromise = new Promise((resolve) => {
    const request = indexedDB.open(DATABASE_NAME, DATABASE_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(STORE_NAME)) {
        database.createObjectStore(STORE_NAME, { keyPath: "runId" });
      }
    };
    request.onsuccess = () => {
      const database = request.result;
      database.onversionchange = () => {
        database.close();
        databasePromise = null;
      };
      resolve(database);
    };
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
  return databasePromise;
}

function requestValue<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error);
    transaction.onabort = () => reject(transaction.error);
  });
}

function validSnapshot(value: unknown, runId: string): value is RunProjectionSnapshot {
  if (!value || typeof value !== "object") return false;
  const snapshot = value as Partial<RunProjectionSnapshot>;
  return snapshot.version === SNAPSHOT_VERSION
    && snapshot.runId === runId
    && Number.isSafeInteger(snapshot.seq)
    && Number(snapshot.seq) > 0
    && !!snapshot.deck
    && snapshot.deck.runId === runId
    && Array.isArray(snapshot.deck.flagConfirmations)
    && !!snapshot.deck.workerPrompts;
}

export async function loadRunProjection(runId: string): Promise<RunProjectionSnapshot | null> {
  const database = await openDatabase();
  if (!database) return null;
  try {
    const transaction = database.transaction(STORE_NAME, "readonly");
    const done = transactionDone(transaction);
    const value = await requestValue(transaction.objectStore(STORE_NAME).get(runId));
    await done;
    if (validSnapshot(value, runId)) return value;
    if (value) void deleteRunProjection(runId);
  } catch {}
  return null;
}

export async function saveRunProjection(runId: string, seq: number, deck: DeckState): Promise<void> {
  if (!runId || !Number.isSafeInteger(seq) || seq <= 0 || deck.runId !== runId) return;
  const database = await openDatabase();
  if (!database) return;
  try {
    const transaction = database.transaction(STORE_NAME, "readwrite");
    const done = transactionDone(transaction);
    const store = transaction.objectStore(STORE_NAME);
    const existing = await requestValue(store.get(runId));
    if (validSnapshot(existing, runId) && existing.seq >= seq) {
      await done;
      return;
    }
    store.put({ runId, seq, deck, savedAt: Date.now(), version: SNAPSHOT_VERSION } satisfies RunProjectionSnapshot);
    const rows = await requestValue(store.getAll()) as RunProjectionSnapshot[];
    const current = rows
      .filter((row) => row.version === SNAPSHOT_VERSION)
      .sort((left, right) => right.savedAt - left.savedAt);
    for (const row of rows) {
      if (row.version !== SNAPSHOT_VERSION) store.delete(row.runId);
    }
    for (const row of current.slice(MAX_SNAPSHOTS)) store.delete(row.runId);
    await done;
  } catch {}
}

export async function deleteRunProjection(runId: string): Promise<void> {
  const database = await openDatabase();
  if (!database) return;
  try {
    const transaction = database.transaction(STORE_NAME, "readwrite");
    const done = transactionDone(transaction);
    transaction.objectStore(STORE_NAME).delete(runId);
    await done;
  } catch {}
}
