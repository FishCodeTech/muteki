import { Type } from "typebox";

const URL_ENV = "MUTEKI_CAPABILITY_ENDPOINT";
const TOKEN_ENV = "MUTEKI_CAPABILITY_TOKEN";
const PROTOCOL = "2025-11-25";

function requiredEnv(name) {
  const value = process.env[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("pi.mcp.env_missing:" + name);
  }
  return value;
}

function parseBody(body, contentType) {
  if (!body) return undefined;
  if (String(contentType || "").includes("text/event-stream")) {
    for (const line of body.split("\n")) {
      const trimmed = line.startsWith("data:") ? line.slice(5).trim() : "";
      if (!trimmed) continue;
      const parsed = JSON.parse(trimmed);
      if (parsed.id !== undefined || parsed.result !== undefined || parsed.error !== undefined) {
        return parsed;
      }
    }
    throw new Error("pi.mcp.empty_sse");
  }
  return JSON.parse(body);
}

function createClient(endpoint, token) {
  let nextId = 1;
  const headers = () => ({
    accept: "application/json, text/event-stream",
    authorization: token.startsWith("Bearer ") ? token : "Bearer " + token,
    "content-type": "application/json",
    "mcp-protocol-version": PROTOCOL,
  });
  const request = async (method, params) => {
    const id = nextId++;
    const response = await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params: params || {} }),
    });
    const body = await response.text();
    if (!response.ok) {
      throw new Error("pi.mcp.http_failed:" + method + ":" + String(response.status));
    }
    const parsed = parseBody(body, response.headers.get("content-type") || "");
    if (!parsed || parsed.error) {
      const message = parsed && parsed.error && parsed.error.message;
      throw new Error(typeof message === "string" && message ? message : "pi.mcp.rpc_failed:" + method);
    }
    return parsed.result;
  };
  const notify = async (method, params) => {
    const response = await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", method, params: params || {} }),
    });
    if (!response.ok && response.status !== 202) {
      throw new Error("pi.mcp.http_failed:" + method + ":" + String(response.status));
    }
  };
  return {
    async connect() {
      await request("initialize", {
        protocolVersion: PROTOCOL,
        capabilities: {},
        clientInfo: { name: "muteki-pi-mcp", version: "1.0.0" },
      });
      await notify("notifications/initialized", {});
    },
    async listTools() {
      const tools = [];
      let cursor = undefined;
      do {
        const result = await request("tools/list", cursor ? { cursor } : {});
        const page = result && result.tools;
        if (!Array.isArray(page)) throw new Error("pi.mcp.tools_list_invalid");
        tools.push(...page);
        cursor = result && result.nextCursor;
      } while (cursor);
      return tools;
    },
    async callTool(name, args, imageInput) {
      return request("tools/call", { name, arguments: args || {},
        ...(typeof imageInput === "boolean"
          ? { _meta: { "muteki/modelCapabilities": { imageInput } } } : {}),
      });
    },
  };
}

function formatResult(result) {
  if (result == null) return "";
  if (typeof result !== "object") return String(result);
  const texts = [];
  if (Array.isArray(result.content)) {
    for (const part of result.content) {
      if (part && part.type === "text" && typeof part.text === "string") texts.push(part.text);
    }
  }
  if (result.structuredContent !== undefined) {
    const structured = JSON.stringify(result.structuredContent);
    if (!texts.includes(structured)) texts.push(structured);
  }
  return texts.length > 0 ? texts.join("\n") : JSON.stringify(result);
}

export default async function mutekiPiMcp(pi) {
  if (typeof Type.Unsafe !== "function") {
    throw new Error("pi.mcp.typebox_unsafe_missing");
  }
  const client = createClient(requiredEnv(URL_ENV), requiredEnv(TOKEN_ENV));
  await client.connect();
  const failedResults = new Map();
  const registeredSignatures = new Map();
  pi.on("tool_result", event => {
    const result = failedResults.get(event.toolCallId);
    if (!result) return;
    failedResults.delete(event.toolCallId);
    return { ...result, isError: true };
  });
  pi.on("agent_end", () => failedResults.clear());
  pi.on("session_shutdown", () => failedResults.clear());

  function registerCatalog(tools) {
    for (const tool of tools) {
      if (!tool || typeof tool.name !== "string" || tool.name.length === 0) {
        throw new Error("pi.mcp.tool_name_invalid");
      }
      const name = tool.name;
      const description = typeof tool.description === "string" && tool.description ? tool.description : name;
      const schema = tool.inputSchema && typeof tool.inputSchema === "object"
        ? tool.inputSchema
        : { type: "object", properties: {} };
      const signature = JSON.stringify({ description, schema });
      if (registeredSignatures.get(name) === signature) {
        continue;
      }
      registeredSignatures.set(name, signature);
      pi.registerTool({
        name,
        label: name,
        description,
        promptSnippet: description.split("\n")[0] || name,
        parameters: Type.Unsafe(schema),
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
          const imageInput = Array.isArray(ctx?.model?.input)
            ? ctx.model.input.includes("image") : undefined;
          const result = await client.callTool(name, params || {}, imageInput);
          const images = result && Array.isArray(result.content)
            ? result.content.filter((part) => part && part.type === "image"
              && typeof part.data === "string" && typeof part.mimeType === "string")
              .map((part) => ({ type: "image", data: part.data, mimeType: part.mimeType }))
            : [];
          const output = {
            content: [{ type: "text", text: formatResult(result) },
              ...(imageInput === false ? [] : images)],
            details: { server: "muteki-control", tool: name, image_input: imageInput },
          };
          if (result && result.isError === true) {
            failedResults.set(_toolCallId, output);
            throw new Error(formatResult(result));
          }
          return output;
        },
      });
    }
  }

  async function refreshTools() {
    const latest = await client.listTools();
    registerCatalog(latest);
    return latest;
  }

  await refreshTools();
  pi.on("before_agent_start", async () => {
    try {
      await refreshTools();
    } catch (_err) {
    }
  });
  pi.on("turn_start", async () => {
    try {
      await refreshTools();
    } catch (_err) {
    }
  });
}
