import type {
  IdentityContextSummary,
  ProxyBridgeStatus,
  RequestMutatorSummary,
  TrafficIndexEntry,
} from "../model";
import { querySrc } from "../../src/reviewPrimitives";
import type { RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);

export function createTrafficDomain(): RealDomain {
  return {
    id: "traffic",
    pollMs: 15000,
    owns: ["trafficItems", "identityContexts", "proxyBridgeStatus", "mutators"],
    mutators: {},
    async load(taskId) {
      const [searchRes, contextsRes, bridgeRes, mutatorsRes] = await Promise.all([
        querySrc<Api>(taskId, "src.traffic.search", { limit: 100 }),
        querySrc<Api>(taskId, "src.identity.contexts", {}),
        querySrc<Api>(taskId, "src.proxy_bridge.status", {}),
        querySrc<Api>(taskId, "src.mutator.list", {}),
      ]);

      const trafficItems: TrafficIndexEntry[] = ((searchRes.items as Api[]) ?? []).map((row) => ({
        exchangeId: text(row.exchange_id),
        method: text(row.method) || "GET",
        observedUrl: text(row.observed_url),
        host: text(row.host),
        path: text(row.path) || "/",
        statusCode: row.status_code != null ? num(row.status_code) : null,
        protocol: text(row.protocol) || "http/1.1",
        contextId: text(row.context_id) || "default",
        sourceRunId: text(row.source_run_id),
        sourceKind: text(row.source_kind) || "http_tool",
        mime: text(row.mime),
        startedAt: text(row.started_at),
        requestSize: num(row.request_size),
        requestSha256: text(row.request_sha256),
        requestSpilled: Boolean(row.request_spilled),
        responseSize: num(row.response_size),
        responseSha256: text(row.response_sha256),
        responseSpilled: Boolean(row.response_spilled),
        bookmarked: Boolean(row.bookmarked),
        bookmarkLabel: text(row.bookmark_label),
      }));

      const identityContexts: IdentityContextSummary[] = ((contextsRes.items as Api[]) ?? []).map((row) => ({
        contextId: text(row.context_id),
        accountRef: text(row.account_ref),
        role: text(row.role),
        tenant: text(row.tenant),
        credentialRef: text(row.credential_ref),
        status: text(row.status) || "unverified",
        sessionFingerprint: text(row.session_fingerprint),
        cookieCount: num(row.cookie_count),
        headerNames: ((row.header_names as unknown[]) ?? []).map((h) => text(h)).filter(Boolean),
        cookieMeta: ((row.cookie_meta as Api[]) ?? []).map((c) => ({
          name: text(c.name),
          domain: text(c.domain),
          path: text(c.path) || "/",
          secure: Boolean(c.secure),
          httpOnly: Boolean(c.http_only),
        })),
        totpConfigured: Boolean(row.totp_configured),
        effectiveStatus: text(row.effective_status) || undefined,
        expiredCookieCount: row.expired_cookie_count != null ? num(row.expired_cookie_count) : undefined,
        lastProbeAt: text(row.last_probe_at) || undefined,
        lastVerifiedAt: text(row.last_verified_at) || undefined,
        lastProbeStatusCode: row.last_probe_status_code != null ? num(row.last_probe_status_code) : null,
      }));

      const proxyBridgeStatus: ProxyBridgeStatus | null = bridgeRes
        ? {
            listening: Boolean(bridgeRes.listening),
            host: text(bridgeRes.host) || "127.0.0.1",
            port: num(bridgeRes.port),
            proxyUsername: text(bridgeRes.proxy_username),
            proxyPassword: text(bridgeRes.proxy_password),
            proxyUrl: text(bridgeRes.proxy_url),
            caFingerprintSha256: text(bridgeRes.ca_fingerprint_sha256),
            captureContextId: text(bridgeRes.capture_context_id),
            inScopeCount: num(bridgeRes.in_scope_count),
            bypassedOutOfScopeCount: num(bridgeRes.bypassed_out_of_scope_count),
            bookmarks: ((bridgeRes.bookmarks as Api[]) ?? []).map((b) => ({
              exchangeId: text(b.exchange_id),
              label: text(b.label),
              updatedAt: text(b.updated_at),
            })),
          }
        : null;

      const mutators: RequestMutatorSummary[] = ((mutatorsRes.items as Api[]) ?? []).map((row) => ({
        mutatorId: text(row.mutator_id),
        name: text(row.name),
        kind: (text(row.kind) === "sandbox_js" ? "sandbox_js" : "declarative") as "declarative" | "sandbox_js",
        algorithm: text(row.algorithm),
        secretRef: text(row.secret_ref),
        outputHeader: text(row.output_header),
        outputQuery: text(row.output_query),
        timestampHeader: text(row.timestamp_header),
        nonceHeader: text(row.nonce_header),
        jsSource: text(row.js_source),
        updatedAt: text(row.updated_at),
      }));

      return { trafficItems, identityContexts, proxyBridgeStatus, mutators };
    },
  };
}
