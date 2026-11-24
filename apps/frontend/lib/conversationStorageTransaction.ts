import { conversationStorageKey } from "./conversationStorageScope";

export function conversationStorageVersion(value: unknown): string | undefined {
  return JSON.stringify(value, (_key, row) => row && typeof row === "object" && !Array.isArray(row)
    ? Object.fromEntries(Object.keys(row).sort().map(key => [key, row[key]])) : row);
}

let opening: Promise<IDBDatabase> | null = null;
function storageDatabase(): Promise<IDBDatabase> {
  if (!opening) {
    opening = new Promise((resolve, reject) => {
      const request = indexedDB.open("muteki:conversation-storage-coordinator", 1);
      let failed = false;
      request.onupgradeneeded = () => { request.result.createObjectStore("transactions"); };
      request.onerror = () => { failed = true; reject(request.error); };
      request.onblocked = () => { failed = true; reject(new DOMException("跨窗口存储事务正在等待其他页面释放。", "InvalidStateError")); };
      request.onsuccess = () => {
        const database = request.result;
        if (failed) { database.close(); return; }
        database.onversionchange = () => { database.close(); opening = null; };
        resolve(database);
      };
    });
    void opening.catch(() => { opening = null; });
  }
  return opening;
}

type StorageOperation<T> = (assertActive: () => void) => T | Promise<T>;
function indexedTransaction<T>(database: IDBDatabase, key: string, run: StorageOperation<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const transaction = database.transaction("transactions", "readwrite");
    const store = transaction.objectStore("transactions");
    let active = true, started = false, settled = false, failed = false;
    let value: T, failure: unknown;
    const assertActive = () => {
      if (!active) throw new Error("conversation.storage.transaction_aborted: 本地存储事务已中断，请重试保存");
    };
    transaction.oncomplete = () => {
      active = false;
      if (failed) reject(failure);
      else if (settled) resolve(value);
      else reject(new Error("conversation.storage.transaction_ended: 保存操作尚未完成"));
    };
    transaction.onerror = () => { active = false; };
    transaction.onabort = () => { active = false; reject(transaction.error || new DOMException("本地存储事务被中断。", "AbortError")); };
    const pulse = () => {
      const request = store.get(key);
      request.onsuccess = () => {
        if (!active) return;
        if (!started) {
          started = true;
          try {
            void Promise.resolve(run(assertActive)).then(result => { value = result; settled = true; }, error => { failure = error; failed = true; settled = true; });
          } catch (error) { failure = error; failed = true; settled = true; }
        }
        if (!settled) pulse();
      };
    };
    pulse();
  });
}

export async function conversationStorageTransaction<T>(scope: string, run: StorageOperation<T>): Promise<T> {
  const key = conversationStorageKey("muteki:conversation-storage", scope);
  const locks = typeof navigator !== "undefined" ? navigator.locks : undefined;
  const execute = async (): Promise<T> => {
    let database: IDBDatabase;
    try { database = await storageDatabase(); }
    catch (error) {
      if (locks) return run(() => {});
      throw new Error(`conversation.storage.coordination_unavailable: 请允许此站点的本地存储后重试保存；${error instanceof Error ? error.message : String(error)}`);
    }
    return indexedTransaction(database, key, run);
  };
  return locks ? locks.request(key, execute) : execute();
}
