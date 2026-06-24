# Muteki Agent — NYU CTF Bench 测评总报告（最终）

> 日期:2026-06-11 · 题库:NYU CTF Bench `test` 集（CSAW 2017–2023,共 **200 题**）
> 对象:muteki autonomous swarm — 异构三引擎 race + 图调度协调器动态扩容
> 单题预算:30 分钟封顶（拿到 flag 即停） · 判定:muteki 报出的 flag 与 challenge.json ground-truth **逐字核对**

---

## 1. 一句话结论

全量跑完 **200 题**（覆盖六大类、难度段 diff 22→74、CSAW 全部 7 个年份）。**200/200 = 100% 全部解出**,无一道因"解题能力不足"失败。难度榜单 36 道高难题全部拿下,含榜首 pwn2own 级 V8 引擎 pwn。

---

## 2. 测评覆盖

| | 数量 |
|---|---|
| 全集 | **200 题** |
| **已测** | **200 题** |
| └ SOLVED | 200（**100%**） |

### 按类覆盖
| 类 | 已测 / 全集 | 解出 |
|----|----|----|
| web | 19 / 19 | 19 |
| pwn | 39 / 39 | 39 |
| rev | 51 / 51 | 51 |
| crypto | 52 / 52 | 52 |
| forensics | 15 / 15 | 15 |
| misc | 24 / 24 | 24 |

---

## 3. 用了什么 worker

每道题派 **3 个异构引擎 worker** race（claude + codex + cursor 同跑同一题,谁先拿到 gate 通过的 flag 谁赢,其余取消）。协调器按"图变化"动态扩容,难题自动加 worker。

| 指标 | 值 |
|------|-----|
| 起步 worker | 3（每引擎 1） |
| 每题 worker 数 | 中位 **4**,范围 **2–15**(难题扩到更多) |
| 总 worker 次（200 题累计） | **912** |
| 上限 | 每题 max 8（部分难题协调器扩到 10–15） |

### 解出贡献（winner 引擎，200 道 SOLVED）
| 引擎 | 解出题数 |
|------|------|
| **cursor** | 80 |
| **claude** | 75 |
| **codex** | 45 |

- **cursor** 总数最多:web/rev/misc/部分 crypto 常秒级抢先。
- **claude** 拿细致逆向/隐写/多步交互 + **pwn2own 级 V8 pwn** + 复杂 forensics。
- **codex** 专啃硬核数学/VM:ECC、Gröbner 基、LSB/lattice、自定义并行 VM / KVM / Heaven's Gate、backdoor RNG。
- **异构价值被实证**:三引擎盲区不重叠,合起来六大类全胜;还出现过"一个引擎卡住(claude 因后端没暴露举手),另一个绕路解出(cursor)"的韧性案例。

---

## 4. 用了什么模型

muteki 的 worker 是 **shelled 订阅版 CLI**,各跑自己 CLI 的满血默认模型（不是 muteki 钉死的固定 model ID）。本次评测机器上的实际版本:

| 引擎 worker | CLI 版本 | 实际模型 |
|------|------|------|
| **claude** | Claude Code 2.1.170 | **Claude Opus 4.7**（1M 上下文,`claude-opus-4.7[1m]`） |
| **codex** | codex-cli 0.138.0 | **GPT-5.5**（`model_reasoning_effort=high`） |
| **cursor** | cursor-agent 2026.06.11 | cursor 默认模型（未钉 `MUTEKI_CURSOR_MODEL`） |
| **Reason 协调器** | （内部 LLM） | **deepseek-v4-pro**（规划/派发循环的决策大脑） |
| 节点摘要/辅助 | （内部） | deepseek-v4-flash |

> 即:解题主力是 **Opus 4.7 / GPT-5.5 / cursor 默认** 三模型异构 race,协调大脑是 **deepseek-v4-pro**。

---

## 5. 成本 / 用时 / token

| 指标 | 值 |
|------|-----|
| 累计成本 | **$214.37**（偏低估,见下） |
| 累计 token | **370M** |
| 解出题用时 | 中位约 **2–4 分钟**,最快 22s,最慢 1708s |
| 高难题平均成本 | ~$1.4/题,方差大(misc/web $0–0.5;pwn/复杂 crypto $2–9) |

**成本偏低估说明**:部分题 cost 显示 $0,因为 (a) cursor 引擎免费;(b) winner 解出后秒杀 loser,其末尾 usage 事件被 stdout 截断(已知 telemetry 限制)。真实 API 等价成本应高于 $214.37。token(370M)受截断影响较小,更可靠。

---

## 6. 测评的实质产出 —— 3 个 muteki bug 修复

- **BUG-1 / BUG-2（flag token 边界,已修+提交 `33c6ca3`）**:`_FLAG_LINE` 的 `\S+` 把同行 markdown `**`/中文叙述吞进 token,或在 flag 内空格处截断（含空格 flag 永远注册不了,还因 never-give-up 空烧 $14/28min）。修复:`_clean_flag_token()` 优先抓完整 `xxx{...}`(允许内部空格)。本批高难题 **3 次实战验证**(bdos / kvm 双空格 / no_time_to_register)。
- **BUG-3（提交闸门假退避,已合 main）**:读到文档里"burn-lockout"字样误触发全局退避,三重门修复。

---

## 7. 难度分析（高难题定位）

题库无现成难度字段,从 CSAW **官方计分**反推:静态分 points + 动态分 decay（decay 越小=预期解出队越少=越难）+ Finals>Quals + 类型先验。综合 `diff≥60` 定为高难,共 **36 道**。

**muteki 在高难题上:36/36 = 100% solved**。覆盖榜首 es1337(74,V8 pwn)、cell(72)、lost_mind(71,RSA LSB)、chatterbox(Windows pwn)等。详见 `difficulty_analysis.md` + `hard_eval_report.md`。

