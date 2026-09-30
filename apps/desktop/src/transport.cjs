const { normalizeOrigin } = require('./policy.cjs');

class DesktopError extends Error {
  constructor(code, message, { retryable = false, source = 'desktop', cause } = {}) {
    super(message, { cause }); this.name = 'DesktopError'; this.code = code;
    this.retryable = retryable; this.source = source;
  }
}

// The local renderer can use the service's existing contracts. It cannot pick
// an arbitrary network destination, redirect its credentials, or proxy a URL.
const API_PATHS = [
  /^\/api\/(health|readiness|workspace-kinds|engines|agent-runtimes)(\/|$)/,
  /^\/api\/auth\/(login|me|ticket)$/,
  /^\/api\/(threads|projects|workspaces|import|sidebar-preferences|receipts|usage)(\/|$)/,
  /^\/api\/directories\/select$/,
  /^\/api\/settings\/(credentials|credential-accounts|model-endpoints|workers|worker-model|worker-models|worker-image|profiles|runtime|llm|extensions|agent-extensions|conversation|notifications|capabilities|operations)(\/|$)/,
  /^\/api\/(conversation|agent-runtime|capabilities|extensions|platform-extensions|operations)(\/|$)/,
  /^\/api\/(agent-extensions|chat-plugins|conversation-shares|commands|queries|effects)(\/|$)/,
  /^\/api\/settings\/(agent-engines|credential-models|identity|system-login|system-update)(\/|$)/,
  /^\/api\/events\/(read|wait)$/,
];
const METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS']);
const REQUEST_HEADERS = new Set(['accept', 'authorization', 'content-type', 'last-event-id', 'idempotency-key']);
const RESPONSE_DROP = new Set(['connection', 'keep-alive', 'transfer-encoding', 'set-cookie', 'content-encoding', 'content-length']);

function allowedApiPath(pathname) {
  return !/%2f|%5c/i.test(pathname) && !pathname.includes('\\')
    && API_PATHS.some(pattern => pattern.test(pathname));
}
function transportError(error) {
  return { code: error.code || 'desktop.operation_failed', message: error.message,
    retryable: error.retryable === true, source: error.source || 'desktop' };
}
function errorResponse(error, status = 502) {
  return Response.json({ ok: false, error: transportError(error) }, { status,
    headers: { 'Access-Control-Allow-Origin': 'muteki-desktop://app',
      'X-Content-Type-Options': 'nosniff' } });
}

async function probeService(value, signal) {
  const origin = normalizeOrigin(value);
  let response;
  try { response = await fetch(`${origin}/api/health`, { signal, redirect: 'error' }); }
  catch (cause) { throw new DesktopError('desktop.connection_failed', '无法读取工作台健康状态，请检查服务地址与连接。', { retryable: true, cause }); }
  if (!response.ok) throw new DesktopError('desktop.health_http', `工作台健康检查返回 HTTP ${response.status}。`, { retryable: true, source: 'service' });
  let health;
  try { health = await response.json(); }
  catch (cause) { throw new DesktopError('desktop.health_protocol', '工作台健康检查没有返回有效 JSON。', { source: 'service', cause }); }
  if (!health || typeof health !== 'object' || (!['ok', 'ready'].includes(health.status) && !(health.status === 'degraded' && health.ready === true))) {
    throw new DesktopError('desktop.service_not_ready', '工作台尚未就绪，请等待后重试。', { retryable: true, source: 'service' });
  }
  return { origin, health };
}

async function forwardService(request, scope, onIdentity) {
  const incoming = new URL(request.url);
  if (!scope || incoming.host !== scope.host || scope.closed) {
    return errorResponse(new DesktopError('desktop.connection_changed', '服务连接已改变，请在当前工作台重试。'), 409);
  }
  if (!METHODS.has(request.method) || !allowedApiPath(incoming.pathname)) {
    return errorResponse(new DesktopError('desktop.operation_unavailable', '此服务操作不属于桌面聊天接口。'), 403);
  }
  if (request.method === 'OPTIONS') return new Response(null, { status: 204, headers: {
    'Access-Control-Allow-Origin': 'muteki-desktop://app',
    'Access-Control-Allow-Methods': [...METHODS].join(', '),
    'Access-Control-Allow-Headers': [...REQUEST_HEADERS].join(', '),
  } });
  const headers = new Headers();
  for (const [key, value] of request.headers) if (REQUEST_HEADERS.has(key.toLowerCase())) headers.set(key, value);
  headers.set('Accept-Encoding', 'identity');
  const target = new URL(incoming.pathname + incoming.search, scope.origin);
  const abort = new AbortController(); scope.requests.add(abort);
  const cancelled = () => abort.abort(request.signal.reason);
  request.signal.addEventListener('abort', cancelled, { once: true });
  if (request.signal.aborted) cancelled();
  try {
    const body = ['GET', 'HEAD'].includes(request.method) ? undefined : request.body;
    const upstream = await fetch(target, { method: request.method, headers, body,
      ...(body ? { duplex: 'half' } : {}), signal: abort.signal, redirect: 'manual' });
    if (scope.closed) throw new DesktopError('desktop.connection_changed', '服务连接已改变，请在当前工作台重试。');
    if (upstream.status >= 300 && upstream.status < 400) {
      throw new DesktopError('desktop.api_redirect', '工作台 API 返回重定向；桌面没有向重定向目标投递登录身份。', { source: 'service' });
    }
    if (upstream.ok && ['/api/auth/me', '/api/auth/login'].includes(incoming.pathname)) {
      const data = await upstream.clone().json();
      if (typeof data.service_id === 'string' && typeof data.identity_id === 'string') onIdentity(scope, data);
    }
    if (scope.closed) throw new DesktopError('desktop.connection_changed', '服务连接已经改变，请在当前工作台重新确认身份。');
    const responseHeaders = new Headers();
    for (const [key, value] of upstream.headers) if (!RESPONSE_DROP.has(key.toLowerCase())) responseHeaders.set(key, value);
    responseHeaders.set('Access-Control-Allow-Origin', 'muteki-desktop://app');
    responseHeaders.set('X-Content-Type-Options', 'nosniff');
    // Preserve streaming and complete response bodies. Scope cancellation owns
    // the underlying request until the stream closes, not just until headers.
    const reader = upstream.body?.getReader();
    const finish = () => { scope.requests.delete(abort); request.signal.removeEventListener('abort', cancelled); };
    const stream = reader ? new ReadableStream({
      async pull(controller) {
        try { const result = await reader.read(); if (result.done) { finish(); controller.close(); }
          else controller.enqueue(result.value); }
        catch (error) { finish(); controller.error(error); }
      },
      async cancel(reason) { abort.abort(reason); finish(); await reader.cancel(reason); },
    }) : null;
    if (!reader) finish();
    return new Response(stream, { status: upstream.status, headers: responseHeaders });
  } catch (cause) {
    scope.requests.delete(abort); request.signal.removeEventListener('abort', cancelled);
    const error = cause instanceof DesktopError ? cause : new DesktopError(
      scope.closed ? 'desktop.connection_changed' : 'desktop.service_request_failed',
      scope.closed ? '服务连接已改变，请在当前工作台重试。' : '工作台请求失败，请检查连接后重试。',
      { retryable: !scope.closed, source: 'service', cause });
    return errorResponse(error);
  }
}

function closeScope(scope) {
  if (!scope) return;
  scope.closed = true;
  for (const controller of scope.requests) controller.abort(new DesktopError('desktop.connection_changed', '服务连接已改变。'));
  scope.requests.clear();
}
module.exports = { DesktopError, transportError, errorResponse, allowedApiPath, probeService, forwardService, closeScope };
