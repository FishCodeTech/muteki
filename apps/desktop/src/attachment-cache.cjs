const fs = require('node:fs');
const path = require('node:path');
const { createHash, randomUUID } = require('node:crypto');
const { DesktopError } = require('./transport.cjs');
const CACHE_LIMIT = 256 * 1024 * 1024;
function scopeKey(scope, draftId) {
  if (!scope?.serviceId || !scope.identityId || typeof draftId !== 'string' || !draftId) throw new DesktopError('desktop.attachment_scope_unverified', '登录当前服务后才能保存附件草稿。');
  return createHash('sha256').update(JSON.stringify([scope.origin, scope.serviceId, scope.identityId, draftId])).digest('hex');
}
async function cacheUsage(root) {
  let entries;
  try { entries = await fs.promises.readdir(root, { withFileTypes: true }); }
  catch (error) { if (error.code === 'ENOENT') return 0; throw error; }
  let total = 0;
  // Count the complete owned directory, including metadata and incomplete
  // atomic-write payloads. Crash residue still consumes the user's disk.
  for (const entry of entries) if (entry.isFile()) total += (await fs.promises.stat(path.join(root, entry.name))).size;
  return total;
}
async function saveAttachment(root, scope, input) {
  const key = scopeKey(scope, input?.draftId), bytes = Buffer.from(input.data);
  if (bytes.length > CACHE_LIMIT) throw new DesktopError('desktop.attachment_cache_limit', '附件超过桌面草稿缓存范围，请保留原文件后重选。');
  await fs.promises.mkdir(root, { recursive: true, mode: 0o700 });
  const id = randomUUID(), file = path.join(root, `${id}.bin`), metadata = path.join(root, `${id}.json`);
  const row = { id, scope: key, name: path.basename(String(input.name || 'attachment')), type: String(input.type || 'application/octet-stream'), lastModified: Number(input.lastModified || 0), size: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex'), createdAt: Date.now() };
  const serialized = JSON.stringify(row);
  if (await cacheUsage(root) + bytes.length + Buffer.byteLength(serialized, 'utf8') > CACHE_LIMIT) throw new DesktopError('desktop.attachment_cache_full', '桌面附件缓存已满（含元数据和未完成缓存）。请移除不再需要的草稿附件后重试。');
  try {
    await fs.promises.writeFile(`${file}.tmp`, bytes, { flag: 'wx', mode: 0o600 });
    await fs.promises.writeFile(`${metadata}.tmp`, serialized, { flag: 'wx', mode: 0o600 });
    await fs.promises.rename(`${file}.tmp`, file); await fs.promises.rename(`${metadata}.tmp`, metadata);
    return { id, name: row.name, type: row.type, size: row.size, sha256: row.sha256 };
  } catch (cause) {
    for (const candidate of [file, metadata, `${file}.tmp`, `${metadata}.tmp`]) await fs.promises.rm(candidate, { force: true });
    throw new DesktopError('desktop.attachment_cache_write_failed', '附件草稿保存失败，原输入仍保留，请重试。', { retryable: true, cause });
  }
}
async function restoreAttachment(root, scope, input) {
  if (!/^[0-9a-f-]{36}$/.test(input?.id || '')) throw new DesktopError('desktop.attachment_cache_id_invalid', '附件缓存引用无效。');
  const key = scopeKey(scope, input.draftId), file = path.join(root, `${input.id}.bin`);
  try {
    const metadata = JSON.parse(await fs.promises.readFile(path.join(root, `${input.id}.json`), 'utf8'));
    if (metadata.scope !== key) throw new DesktopError('desktop.attachment_cache_scope_changed', '此附件缓存属于其他工作台或草稿，请重新选择。');
    const bytes = await fs.promises.readFile(file);
    if (metadata.size !== bytes.length || metadata.sha256 !== createHash('sha256').update(bytes).digest('hex')) throw new DesktopError('desktop.attachment_cache_corrupt', '附件缓存已损坏，请重新选择原文件。');
    return { data: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength), name: metadata.name, type: metadata.type, lastModified: metadata.lastModified };
  } catch (cause) { if (cause instanceof DesktopError) throw cause; throw new DesktopError('desktop.attachment_cache_unavailable', '附件草稿缓存无法读取，请重新选择原文件。', { retryable: true, cause }); }
}
async function removeAttachment(root, scope, input) {
  if (!/^[0-9a-f-]{36}$/.test(input?.id || '')) throw new DesktopError('desktop.attachment_cache_id_invalid', '附件缓存引用无效。');
  const key = scopeKey(scope, input.draftId), metadata = path.join(root, `${input.id}.json`);
  let row;
  try { row = JSON.parse(await fs.promises.readFile(metadata, 'utf8')); }
  catch (error) { if (error.code === 'ENOENT') return; throw error; }
  if (row.scope !== key) throw new DesktopError('desktop.attachment_cache_scope_changed', '此附件缓存属于其他工作台或草稿。');
  await fs.promises.rm(path.join(root, `${input.id}.bin`), { force: true });
  await fs.promises.rm(metadata, { force: true });
}
module.exports = { saveAttachment, restoreAttachment, removeAttachment, cacheUsage, CACHE_LIMIT };
