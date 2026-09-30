const { DesktopError } = require('./transport.cjs');

class NativeSpeech {
  constructor({ platform, load, send }) { this.platform = platform; this.load = load; this.send = send; this.active = null; this.native = null; }
  current(entry) {
    const record = entry.record;
    return this.active === entry && !record.window.isDestroyed() && record.scope === entry.scope && !entry.scope.closed && record.state.connectionVersion === entry.connectionVersion && record.state.route === entry.route;
  }
  start(record, input) {
    if (this.platform !== 'darwin') throw new DesktopError('desktop.speech.unsupported', '当前桌面平台未提供 Apple 语音识别。');
    const scope = record.scope;
    if (record.remote || !/^\/chat(?:\/[^/?#]+)?(?:[?#]|$)/.test(record.state.route || '')) throw new DesktopError('desktop.speech.chat_required', '请在聊天页面开始语音输入。');
    if (!scope?.serviceId || scope.closed || input?.serviceId !== scope.serviceId || input?.identityId !== scope.identityId || input?.connectionVersion !== record.state.connectionVersion) throw new DesktopError('desktop.speech.scope_changed', '语音输入所属的工作台已经改变。');
    if (typeof input.id !== 'string' || !/^[a-zA-Z0-9-]{16,80}$/.test(input.id) || typeof input.scopeKey !== 'string' || !input.scopeKey || typeof input.locale !== 'string' || !/^[a-zA-Z]{2,3}(?:-[a-zA-Z0-9]{2,8})*$/.test(input.locale)) throw new DesktopError('desktop.speech.invalid_request', '语音输入请求无效。');
    if (this.active?.record === record) this.cancel(record);
    if (this.active) throw new DesktopError('desktop.speech.busy', '另一个窗口正在使用麦克风，请先结束该录音。');
    if (!this.native) {
      try { this.native = this.load(); }
      catch (error) { throw new DesktopError('desktop.speech.module_unavailable', `macOS 语音识别模块无法加载。${error.message}`); }
    }
    const entry = { record, scope, route: record.state.route, connectionVersion: record.state.connectionVersion, id: input.id, scopeKey: input.scopeKey };
    this.active = entry;
    try {
      this.native.start(input.locale, raw => {
        if (!this.current(entry)) { if (this.active === entry) this.cancel(record, { id: entry.id }); return; }
        let event;
        try { event = JSON.parse(raw); }
        catch (error) { event = { type: 'error', code: 'desktop.speech.invalid_event', message: '原生语音识别返回无效事件。', detail: { raw, error: error.message } }; }
        if (!['requesting', 'listening', 'partial', 'processing', 'result', 'error', 'cancelled'].includes(event.type) || (['partial', 'result'].includes(event.type) && typeof event.text !== 'string')) event = { type: 'error', code: 'desktop.speech.invalid_event', message: '原生语音识别返回无效事件。', detail: { raw } };
        const terminal = ['result', 'error', 'cancelled'].includes(event.type);
        if (terminal) this.active = null;
        this.send(record, { ...event, id: entry.id, scopeKey: entry.scopeKey, connectionVersion: entry.connectionVersion, serviceId: scope.serviceId, identityId: scope.identityId });
        if (event.code === 'desktop.speech.invalid_event') this.native.cancel();
      });
    } catch (error) { this.active = null; throw new DesktopError(error.code || 'desktop.speech.start_failed', error.message); }
    return { id: entry.id };
  }
  finish(record, input) {
    const entry = this.active;
    if (!entry || entry.record !== record || entry.id !== input?.id || !this.current(entry)) throw new DesktopError('desktop.speech.session_changed', '录音会话已结束或改变。');
    this.native.finish();
  }
  cancel(record, input) {
    const entry = this.active;
    if (!entry || entry.record !== record || (input?.id && entry.id !== input.id)) return;
    this.native.cancel();
    if (this.active !== entry) return;
    this.active = null;
    if (!record.window.isDestroyed()) this.send(record, { type: 'cancelled', id: entry.id, scopeKey: entry.scopeKey, connectionVersion: entry.connectionVersion, serviceId: entry.scope.serviceId, identityId: entry.scope.identityId });
  }
}
module.exports = { NativeSpeech };
