const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');

// The renderer never receives or stores a bearer token. Native sessions are
// keyed by both the requested address and the service's installation identity.
class ServiceAuthSessions {
  constructor(file, safeStorage, onChange = () => {}) {
    this.file = file; this.safeStorage = safeStorage; this.onChange = onChange;
    this.sessions = new Map(); this.saved = new Map(); this.restoreWarning = '';
    try {
      if (!fs.existsSync(file)) return;
      const data = JSON.parse(fs.readFileSync(file, 'utf8'));
      if (data.version !== 1 || !Array.isArray(data.entries)) throw new Error('invalid session cache');
      for (const entry of data.entries) {
        if (!entry || typeof entry.origin !== 'string' || typeof entry.serviceId !== 'string' || typeof entry.encrypted !== 'string') throw new Error('invalid session entry');
        this.saved.set(this.key(entry.origin, entry.serviceId), entry);
      }
    } catch {
      this.restoreWarning = '记住的登录无法读取；请重新登录。新的会话会在保存成功后恢复记住登录。';
    }
  }
  key(origin, serviceId) { return `${origin}|${serviceId}`; }
  canEncrypt() {
    try {
      return this.safeStorage.isEncryptionAvailable()
        && !(process.platform === 'linux' && this.safeStorage.getSelectedStorageBackend() === 'basic_text');
    } catch { return false; }
  }
  forService(origin, serviceId) {
    const key = this.key(origin, serviceId);
    if (this.sessions.has(key)) return this.sessions.get(key);
    const owner = { origin, serviceId, token: '', expiresAt: 0, revision: 0,
      persistenceWarning: this.restoreWarning,
      snapshot: () => ({ token: owner.token, revision: owner.revision }),
      accept: (data, remember, source) => this.accept(owner, data, remember, source),
      clear: (snapshot, source) => this.clear(owner, snapshot, source) };
    const entry = this.saved.get(key);
    if (entry) {
      try {
        if (!this.canEncrypt()) throw new Error('secure storage unavailable');
        const data = JSON.parse(this.safeStorage.decryptString(Buffer.from(entry.encrypted, 'base64')));
        if (data.origin !== origin || data.serviceId !== serviceId || typeof data.token !== 'string' || !Number.isFinite(data.expiresAt)) throw new Error('invalid private session');
        if (data.expiresAt > Date.now() / 1000) { owner.token = data.token; owner.expiresAt = data.expiresAt; }
      } catch {
        owner.persistenceWarning = '记住的登录无法解密，请重新登录。';
      }
    }
    this.sessions.set(key, owner);
    return owner;
  }
  persist() {
    const temporary = `${this.file}.${randomUUID()}.tmp`;
    try {
      fs.mkdirSync(path.dirname(this.file), { recursive: true, mode: 0o700 });
      fs.writeFileSync(temporary, JSON.stringify({ version: 1, entries: [...this.saved.values()] }), { mode: 0o600 });
      fs.renameSync(temporary, this.file);
      fs.chmodSync(this.file, 0o600);
    } finally { if (fs.existsSync(temporary)) fs.unlinkSync(temporary); }
  }
  accept(owner, data, remember, source) {
    if (data.session_protocol !== 2 || data.service_id !== owner.serviceId || data.session_owner !== 'desktop'
        || typeof data.auth_required !== 'boolean' || (data.auth_required && (typeof data.token !== 'string' || !data.token || !Number.isFinite(data.expires_at)))) {
      const error = new Error('服务登录协议或身份不匹配，请重新连接当前服务。');
      error.code = 'desktop.auth_protocol'; throw error;
    }
    owner.token = data.auth_required ? data.token : '';
    owner.expiresAt = data.auth_required ? data.expires_at : 0;
    owner.revision += 1; owner.persistenceWarning = '';
    const key = this.key(owner.origin, owner.serviceId);
    this.saved.delete(key);
    if (remember && owner.token) {
      if (this.canEncrypt()) {
        try {
          const plaintext = JSON.stringify({ origin: owner.origin, serviceId: owner.serviceId, token: owner.token, expiresAt: owner.expiresAt });
          const encrypted = this.safeStorage.encryptString(plaintext).toString('base64');
          this.saved.set(key, { origin: owner.origin, serviceId: owner.serviceId, encrypted });
        } catch { owner.persistenceWarning = '登录仅保存在本次桌面运行中：系统安全存储不可用。'; }
      } else owner.persistenceWarning = '登录仅保存在本次桌面运行中：系统安全存储不可用。';
    }
    try { this.persist(); }
    catch { owner.persistenceWarning = '登录仅保存在本次桌面运行中：会话文件保存失败。'; this.saved.delete(key); }
    this.onChange(owner, source);
  }
  clear(owner, snapshot, source) {
    // An old request may clear only the token it actually used.
    if (snapshot && (snapshot.revision !== owner.revision || snapshot.token !== owner.token)) return;
    owner.token = ''; owner.expiresAt = 0; owner.revision += 1;
    this.saved.delete(this.key(owner.origin, owner.serviceId));
    try { this.persist(); owner.persistenceWarning = ''; }
    catch { owner.persistenceWarning = '本次登录已退出，但本机会话文件清理失败；服务已撤销的令牌无法再次登录。'; }
    this.onChange(owner, source);
  }
}
module.exports = { ServiceAuthSessions };
