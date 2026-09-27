/** User-facing copy for first-load / refresh thread failures. */
export function threadLoadFailureMessage(httpStatus: number): string {
  if (httpStatus === 0) {
    return "网络不可用，无法加载对话。请检查连接后重试。";
  }
  if (httpStatus === 404) {
    return "找不到该对话（可能已删除或链接无效）。";
  }
  if (httpStatus === 401 || httpStatus === 403) {
    return "没有权限查看该对话，请重新登录或确认访问权限。";
  }
  if (httpStatus >= 500) {
    return `服务暂时不可用，请稍后重试（HTTP ${httpStatus}）。`;
  }
  if (httpStatus > 0) {
    return `加载对话失败（HTTP ${httpStatus}）。`;
  }
  return "加载对话失败，请重试。";
}

export function throwThreadLoadFailure(httpStatus: number): never {
  const error = new Error(threadLoadFailureMessage(httpStatus)) as Error & {
    httpStatus?: number;
  };
  error.httpStatus = httpStatus;
  throw error;
}

export function rethrowNetworkLoadFailure(exc: unknown): never {
  if (
    exc instanceof TypeError
    || (
      exc instanceof Error
      && /Failed to fetch|NetworkError|network request failed|Load failed|ECONNREFUSED|ERR_CONNECTION/i.test(
        exc.message,
      )
    )
  ) {
    throwThreadLoadFailure(0);
  }
  throw exc;
}
