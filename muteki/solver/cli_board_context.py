"""Blackboard/context prompt assembly for CliSolver. Moved from cli_solver.py."""
from __future__ import annotations

import asyncio
import json
import re
import time
from pathlib import Path
from typing import Optional

from muteki.core.prompt_assembly import PromptPart, compile_prompt, resolve_prompt_budget
from muteki.solver.context_manifest import ContextManifest
from muteki.solver.workspace import (
    ensure_workspace,
    relative_symlink,
    workspace_root_for_worker,
)
from muteki.solver.graph_text import ContextRenderError
from muteki.solver.cli_prompts import (
    _EXEC_PROMPT,
    _EXPLORE_PROMPT,
    _CTF_FACT_ASSERTION_NOTE,
    _CTF_FACT_VERIFY_PROMPT,
    _FACT_VERIFY_PROMPT,
    _KB_PROMPT,
    _PENTEST_FGS_EXPLORE_PROMPT,
    _REVIEW_PROMPT,
    without_operator_input_capability,
)

# Per-prompt ContextManifest snapshot, written into the worker cwd next to the
# board file (operators/diff tooling; the deck gets the same payload as a
# blackboard delta). Per-WORKER, unlike the shared board file.
_CONTEXT_MANIFEST_FILENAME = ".muteki_context_manifest.json"

# Offline black-box eval boundary, shared by the bootstrap and explore paths.
_OFFLINE_BOUNDARY_BLOCK = (
    "\n## Offline black-box evaluation boundary\n"
    "Use only the challenge target and files already staged in your "
    "current working directory. Do NOT inspect parent directories, "
    "benchmark metadata, challenge manifests, README solution text, "
    "reference solvers, generators containing answers, other run logs, "
    "or agent/session history. If a required player-facing file is not "
    "staged here, report it missing instead of searching the host."
)

def _join_sections(sections: "list[tuple[str, str, dict]]") -> str:
    """The ctx string handed to the prompt template: section texts joined with
    newlines (identical layout to the old flat ctx_lines assembly)."""
    return "\n".join(text for _name, text, _meta in sections)


def _task_reference_block(self) -> str:
    if getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        instruction = str(
            getattr(
                getattr(self.challenge, "task_contract", None),
                "raw_instruction",
                "",
            )
            or getattr(self.challenge, "description", "")
            or f"Solve {self.challenge.name} [{self.challenge.category}]"
        ).strip()
        return f"\n## Goal\n{instruction}"
    name = str(getattr(self, "_task_file_name", "") or "")
    digest = str(getattr(self, "_task_file_sha256", "") or "")
    if not name or not digest:
        raise RuntimeError("complete operator task was not staged")
    return (
        "\n## Complete operator task\n"
        f"Read `./{name}` in full before doing any work. "
        f"Its SHA-256 is `{digest}`. The file is the authoritative operator "
        "instruction; the fields above are only its execution contract."
    )


def _compile_worker_prompt(
    self, role: str, fixed_template: str,
    sections: "list[tuple[str, str, dict]]",
) -> str:
    if not bool(getattr(self.challenge, "allow_operator_input", True)):
        fixed_template = without_operator_input_capability(fixed_template)
    if getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        if bool(getattr(self.challenge, "allow_operator_input", True)) and role in {
            "worker_bootstrap", "worker_explore",
        }:
            fixed_template += (
                "\n\n只有确实缺少必须由操作者提供的外部信息或资源，且无法自行继续时，"
                "才用 `python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" request-input "
                "'<明确需要的信息>'` 请求人工输入。不要用它代替分析或尝试。"
            )
        prompt = fixed_template.replace(
            "{context}", _join_sections(sections), 1
        )
        self._persist_context_manifest(role, sections, omissions=[])
        return prompt
    if getattr(self.challenge, "mode", "ctf") == "pentest":
        fixed_template += (
            "\n\n## Network activity limits\n"
            "Do not start with scanning or brute force. Read supplied files and existing "
            "team evidence first. Do not run full-port scans, subnet sweeps, bulk wordlist "
            "enumeration, or online password guessing unless the operator explicitly "
            "requests that activity. Do not replace these with equivalent scripts.\n"
            "If network requests repeatedly fail or the team has instructed you to stop, "
            "stop network activity and report the blocker; do not broaden or repeat it.\n"
        )
    required_names = {
        "header", "task-instruction", "report-instruction", "workspace-protocol",
        "standing-guidance", "toolbox", "shared-access", "shared-state",
    }
    required = tuple(
        PromptPart(name, text, external_files=tuple(meta.get("external_files") or ()))
        for name, text, meta in sections
        if name in required_names and text and text.strip()
    )
    optional_parts: list[PromptPart] = []
    for name, text, meta in sections:
        if name in required_names or not text or not text.strip():
            continue
        ids = [str(value) for value in (
            meta.get("fact_seqs") or meta.get("intent_ids")
            or meta.get("artifact_ids") or []
        )]
        lines = [line for line in text.splitlines() if line.strip()]
        if len(lines) <= 1:
            optional_parts.append(PromptPart(
                name, text, omission_group=name,
                item_id=ids[0] if ids else "",
                external_files=tuple(meta.get("external_files") or ()),
            ))
            continue
        for index, line in enumerate(lines):
            optional_parts.append(PromptPart(
                name, line, omission_group=name,
                item_id=ids[index] if index < len(ids) else "",
                external_files=tuple(meta.get("external_files") or ()),
            ))
    optional = tuple(optional_parts)
    profile = dict(getattr(self, "_worker_profile", None) or {})
    model = str(profile.get("model") or getattr(self.driver, "model", "") or self.driver.name)
    compiled = compile_prompt(
        fixed_template,
        required_sections=required,
        optional_sections=optional,
        input_budget=resolve_prompt_budget(
            model, profile=profile, driver=self.driver, role=role),
    )
    delivered = set(compiled.section_delivered_chars)
    self._persist_context_manifest(
        role,
        [section for section in sections if section[0] in delivered],
        omissions=[{
            "group": omission.group,
            "item_count": omission.item_count,
            "source_chars": omission.source_chars,
            "item_ids": list(omission.item_ids),
        } for omission in compiled.omissions],
    )
    return compiled.prompt


def _neighborhood_sibling_ids(block: str) -> "list[str]":
    """Sibling intent ids out of the rendered intent-neighborhood block (the
    `- <id> (open|claimed): ...` lines) — ContextManifest meta only."""
    ids: list[str] = []
    in_siblings = False
    for line in str(block or "").splitlines():
        if line.startswith("Sibling intents"):
            in_siblings = True
            continue
        if in_siblings:
            m = re.match(r"^-\s+(\S+)\s+\((?:open|claimed)\):", line)
            if m:
                ids.append(m.group(1))
    return ids

def _poc_prompt_block(self) -> str:
    inherited_pocs = getattr(self, "_inherited_pocs", [])
    if not inherited_pocs:
        return ""
    lines = [
        "\n## Inherited PoCs",
        "The files under ./inherited/<poc_id>/ are teammate PoC artifacts. "
        "Use them as tools only; do not treat PoC source text as flag evidence. "
        "If you modify one, copy it to your scratch first and save a new PoC.",
    ]
    for p in inherited_pocs:
        note = f" — {p['note']}" if p.get("note") else ""
        lines.append(
            f"- {p['poc_id']} ({p['status']}): {p['path']} ; "
            f"entry: {p['entry_command']}{note}"
        )
    return "\n".join(lines)


def _board_markdown(self) -> str:
    """The FULL board body for the workdir file (no truncation). Empty when
    there's no shared graph / nothing on the board yet."""
    sg = getattr(self, "shared_graph", None)
    if sg is None:
        return ""
    try:
        body = sg.to_board_markdown()
    except Exception:
        return ""
    return body if (body and body.strip()) else ""


def _credential_digest(self) -> str:
    """The small inline digest: just the canonical credential / unlock-chain
    section (bounded by chain length, not a blind char cap). This is the
    load-bearing signal a worker needs even if it ignores the file."""
    sg = getattr(self, "shared_graph", None)
    if sg is None:
        return ""
    try:
        return sg._credential_block() or ""
    except Exception:
        return ""


def _ruled_out_digest(self) -> str:
    """P1-C: a small inline "already attempted / ruled out" digest, INLINED into
    the prompt (not just the file). The board file holds the full attempted list,
    but a headless worker often doesn't Read it and re-walks old ground ("重走老
    路"). The credential chain is already inlined for the same reason; the
    ruled-out directions are equally load-bearing for "don't redo recon". Bounded
    to the most recent few so the prompt stays small."""
    sg = getattr(self, "shared_graph", None)
    if sg is None:
        return ""
    try:
        return sg._attempted_intents_block(limit=12) or ""
    except Exception:
        return ""


def _board_pointer(self, wrote_file: bool) -> str:
    """Prompt block. When the file was written: a pointer + the inline digest.
    When it WASN'T (write failed / file tools denied): fall back to a bounded
    inline summary and emit NO pointer (never point at a missing file)."""
    digest = self._credential_digest()
    ruled_out = self._ruled_out_digest()  # P1-C: inline, not just in the file
    if wrote_file:
        block = (
            "\n## Shared team board (teammates' findings)\n"
            f"A file `./{self.BOARD_FILENAME}` in your working directory holds the "
            "FULL team board — it is your team's shared notes, NOT part of the "
            "challenge.\n"
            "Build on its confirmed facts, reuse recovered "
            "credentials/passwords, and do NOT redo anything it marks a dead end "
            "or already-attempted direction.\n"
            "Before starting a new direction, also query the LIVE board with the "
            "script path in `$MUTEKI_BLACKBOARD_SCRIPT` (this is fresher than the "
            "snapshot):\n"
            "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" context\n"
            "When you confirm a new objective fact, write it immediately:\n"
            "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" write-fact \"<observation>\" "
            "--witness \"<literal output>\"\n"
            "When a direction is ruled out, mark it immediately:\n"
            "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" mark-deadend \"<reason>\" "
            "--tested \"<exact bound>\"\n")
        if digest:
            block += digest
        # P1-C: inline the already-attempted directions too (not only in the file
        # the worker may skip). This is what stops a fresh bootstrap worker from
        # re-running the same recon a previous one already concluded.
        if ruled_out:
            block += ruled_out + "\n"
        return block
    # fallback: no file — inline a bounded summary so the worker isn't blind.
    sg = getattr(self, "shared_graph", None)
    summary = ""
    try:
        summary = (
            sg.to_summary(max_evidence=10**9, max_dead_ends=10**9)
            if sg else ""
        ).strip()
    except Exception:
        summary = ""
    if not summary and not digest:
        return ""
    out = "\n## Shared team board (what your teammates already found)\n"
    if digest:
        out += digest
    if summary:
        out += ("Build on confirmed facts; do NOT re-investigate anything marked a "
                f"dead end.\n{summary}\n")
    if ruled_out:
        out += ruled_out + "\n"
    return out


def _board_context(self) -> str:
    """Board block for the standby RESPOND path (its only remaining caller —
    the solve-role builders assemble role-scoped projections instead and never
    point the model at the board file). Returns the prompt block ASSUMING the
    board file was written (the respond path writes it before building the
    prompt, see _write_board_file); degrades to the inline fallback via
    _board_pointer(wrote_file=False) when no file could be written."""
    return self._board_pointer(bool(getattr(self, "_board_file_written", False)))


def _step_contract_block(self) -> str:
    facts = [int(x) for x in (getattr(self, "from_facts", None) or []) if int(x) > 0]
    obs = str(getattr(self, "expected_observable", "") or "").strip()
    stop = str(getattr(self, "stop_condition", "") or "").strip()
    cov = str(getattr(self, "coverage_key", "") or "").strip()
    required_pocs = [
        str(item) for item in (getattr(self, "required_pocs", None) or [])
        if str(item).strip()
    ]
    lane = str(getattr(self, "lane", "") or "").strip()
    if (not (facts or obs or stop or cov or lane or required_pocs)
            and getattr(self, "mode", "") not in {"explore", "bootstrap"}):
        return ""
    if getattr(getattr(self, "challenge", None), "mode", "ctf") in {"ctf", "pentest"}:
        lines = ["\n## 当前 Step"]
        if facts:
            lines.append("来源 Fact：" + ", ".join(f"#{n}" for n in facts))
        if obs:
            lines.append(f"预期证据：{obs}")
        if stop:
            lines.append(f"停止边界：{stop}")
        if cov:
            lines.append(f"覆盖问题：{cov}")
        if required_pocs:
            lines.append("所需 PoC 资源：" + ", ".join(required_pocs))
        if getattr(getattr(self, "challenge", None), "mode", "ctf") == "pentest":
            claim = dict(getattr(self, "value_claim", {}) or {})
            lines.extend([
                f"授权资产：{claim.get('asset') or ''}",
                f"测试身份：{claim.get('identity') or 'anonymous'}",
                f"风险等级：{claim.get('risk_tier') or 'bounded_validation'}",
                f"授权版本：{claim.get('authorization_version') or 1}",
                f"证据要求：{claim.get('evidence_requirement') or obs}",
            ])
        lines.append(
            "把本 Step 作为一条连贯因果链执行，不把停止边界当成固定请求清单。"
            "出现可能改变结论的新观察时，先完成最小区分试验；只有预期证据已得到，"
            "或有限范围已穷尽且没有未解释观察时，才结束当前 Step。"
        )
        return "\n".join(lines)
    lines = ["\n## Current step contract"]
    if facts:
        lines.append("From facts: " + ", ".join(f"#{n}" for n in facts))
    if obs:
        lines.append(f"Expected observable: {obs}")
    if stop:
        lines.append(f"Stop when: {stop}")
    if cov:
        lines.append(f"Coverage key: {cov}")
    if lane:
        lines.append(f"Authorized exclusive resource: {lane}")
        lines.append(
            "Any shared-state mutation or exclusive action must target only this "
            "resource and must first win claim-resource."
        )
    else:
        lines.append(
            "Authorized exclusive resource: none. Do not mutate shared target state "
            "or start an exclusive action in this step. If the evidence unlocks one, "
            "publish the prerequisite fact and conclude for replanning."
        )
    lines.append(
        "If claim-resource is held by another Worker, conclude immediately; do not "
        "wait, sleep, or retry the lease inside this Worker."
    )
    lines.append(
        "Stay inside this step. When the stop condition is met, conclude "
        "instead of expanding into a new direction."
    )
    return "\n".join(lines)


def _intent_neighborhood_context(self) -> str:
    sg = getattr(self, "shared_graph", None)
    intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or ""
    if sg is None or not intent_id:
        return ""
    try:
        return sg.intent_neighborhood_block(intent_id) or ""
    except Exception:
        return ""


def _source_facts_block(self) -> "tuple[str, dict]":
    """The FULL text of THIS intent's source facts (the ``[#seq]`` refs the
    neighborhood block renders) — the established ground this step builds on,
    without pointing the model at the full board. Returns (block, meta); meta
    carries the fact seqs + artifact ids for the ContextManifest."""
    sg = getattr(self, "shared_graph", None)
    intent_id = (getattr(self, "intent_id_assigned", "")
                 or getattr(self, "_intent_id", "") or "")
    if sg is None or not intent_id:
        return "", {}
    try:
        facts = sg.intent_source_facts(intent_id) or []
    except Exception:
        return "", {}
    if not facts:
        return "", {}
    lines = ["\n## Source facts for this step (already established — build on them)"]
    for f in facts:
        verdict = "verified" if f.get("verified") else "candidate"
        line = f"- [#{int(f.get('seq') or 0)}] ({verdict}) {str(f.get('text') or '')}"
        if f.get("artifact_id"):
            line += f" [artifact:{f['artifact_id']}]"
        lines.append(line)
    meta = {
        "fact_seqs": [int(f.get("seq") or 0) for f in facts
                      if int(f.get("seq") or 0) > 0],
        "artifact_ids": [str(f["artifact_id"]) for f in facts
                         if f.get("artifact_id")],
    }
    return "\n".join(lines), meta


def _source_artifacts_block(self, source_meta: dict) -> "tuple[str, dict]":
    """One line per source-fact artifact id pointing at the existing
    ``[peek:<artifact_id>]`` mechanism — raw evidence stays referenced, not
    inlined. Returns (block, meta) for the ContextManifest."""
    artifact_ids = [str(a) for a in ((source_meta or {}).get("artifact_ids") or [])
                    if str(a or "").strip()]
    if not artifact_ids:
        return "", {}
    lines = [
        "\n## Relevant artifacts (peek to inspect raw evidence)",
        "Each id is the raw captured output behind a source fact — reference "
        "it as [peek:<artifact_id>] when you cite the fact, and inspect the "
        "raw evidence before re-running the same probe.",
    ]
    lines += [f"- {aid}" for aid in artifact_ids]
    return "\n".join(lines), {"artifact_ids": artifact_ids}


def _dead_ends_scoped_block(self, *, epoch_wide: bool = False) -> str:
    """Projected dead-end section. Explore/verifier: scoped to this step
    (intent + coverage/route + epoch). Bootstrap has no assigned intent, so it
    gets the WHOLE current epoch's ruled-out ground (bound or unbound) — what
    stops a fresh rush from re-running recon a prior worker concluded."""
    sg = getattr(self, "shared_graph", None)
    if sg is None:
        return ""
    intent_id = "" if epoch_wide else (
        getattr(self, "intent_id_assigned", "")
        or getattr(self, "_intent_id", "") or "")
    try:
        return sg.dead_ends_context_block(
            intent_id=intent_id,
            coverage_key="" if epoch_wide else str(getattr(self, "coverage_key", "") or ""),
            route_hash="" if epoch_wide else str(getattr(self, "route_hash", "") or ""),
            target_epoch=str(getattr(self, "_target_epoch", "") or ""),
            title=("\n## Dead ends this epoch (already ruled out — do NOT "
                   "re-investigate)" if epoch_wide else ""),
            epoch_wide=epoch_wide) or ""
    except Exception:
        return ""


def _persist_context_manifest(self, role: str,
                              sections: "list[tuple[str, str, dict]]",
                              *, omissions: "Optional[list[dict]]" = None,
                              full_board_included: bool = False
                              ) -> "Optional[ContextManifest]":
    """Build + store + persist + emit the ContextManifest for one assembled
    prompt. Best-effort at EVERY step — manifest IO must never break prompt
    building. The JSON snapshot lands in the worker cwd; the blackboard delta
    feeds the deck (UI tolerates unknown kinds)."""
    try:
        manifest = ContextManifest.build(
            role=role,
            worker_id=str(getattr(self, "solver_id", "") or ""),
            intent_id=(getattr(self, "intent_id_assigned", "")
                       or getattr(self, "_intent_id", "") or ""),
            sections=sections,
            omissions=omissions,
            full_board_included=full_board_included)
    except Exception:
        return None
    self._context_manifest = manifest
    try:
        wd = (getattr(self, "_workdir", None)
              or getattr(self, "_owned_scratch", None))
        if wd:
            cwd = Path(wd)
            cwd.mkdir(parents=True, exist_ok=True)
            (cwd / _CONTEXT_MANIFEST_FILENAME).write_text(
                json.dumps(manifest.to_dict(), ensure_ascii=False, indent=2))
    except Exception:
        pass
    try:
        pending = self._emit_bb("context_manifest", **manifest.to_dict())
        if asyncio.iscoroutine(pending):
            try:
                task = asyncio.get_running_loop().create_task(pending)
                task.add_done_callback(_swallow_emit_error)
            except RuntimeError:
                pending.close()  # no running loop (sync test harness) — drop it
    except Exception:
        pass
    return manifest


def _swallow_emit_error(task: "asyncio.Task") -> None:
    """Retrieve (and discard) a fire-and-forget manifest emit's failure so it
    never surfaces as an unhandled-task warning in the worker loop."""
    try:
        task.exception()
    except BaseException:
        pass


@staticmethod
def _workspace_protocol_block() -> str:
    return (
        "\n## Workspace contract\n"
        "Your current directory is this Worker's private cwd for independent work; "
        "it is not a permission boundary between Workers in the same Run. Put reusable scripts, outputs, and connection "
        "instructions in `./shared/` for explicit handoff, and preserve files other "
        "Workers already placed under `./shared/`. Attached inputs remain available "
        "in the current directory.\n"
    )


def _ctf_shared_state_block(self) -> str:
    """Return the complete CTF collaboration graph in the Decide wire shape."""
    graph = getattr(self, "shared_graph", None)
    if graph is None:
        return ""
    try:
        return str(graph.to_ctf_graph_yaml() or "")
    except ContextRenderError:
        raise
    except Exception as exc:
        raise ContextRenderError(
            f"required shared graph could not be rendered: "
            f"{type(exc).__name__}: {exc}"
        ) from exc


def _toolbox_block(self) -> str:
    manifest = dict(getattr(self, "_toolbox_manifest", None) or {})
    entries = [row for row in manifest.get("entries", []) if isinstance(row, dict)]
    if not entries:
        return ""
    uploadable = [
        (
            f"{row.get('logical_name')} "
            f"({row.get('target_os')}/{row.get('target_arch')}): "
            f"{row.get('path')}"
        )
        for row in entries if row.get("uploadable")
    ]
    commands = [
        f"{row.get('logical_name')}: {row.get('path')}"
        for row in entries
        if row.get("category") == "host-command" and row.get("logical_name")
    ]
    data_dirs = [
        f"{row.get('logical_name')}: {row.get('path')}"
        for row in entries
        if row.get("category") == "data-dir" and row.get("logical_name")
    ]
    lines = [
        "\n## Unified Toolbox",
        "Only paths confirmed present in this Worker image are listed. "
        "Paths are relative to the current working directory. "
        "Choose from the listed tools using the observed target OS/architecture; "
        "a Step that asks for a capability does not prescribe one tool.",
    ]
    if commands:
        lines.append("Host commands: " + ", ".join(commands))
    if data_dirs:
        lines.append("Data directories: " + ", ".join(data_dirs))
    if uploadable:
        lines.append("Uploadable target files: " + ", ".join(uploadable))
    return "\n".join(lines)


def _shared_access_block(self) -> str:
    graph = getattr(self, "shared_graph", None)
    if graph is None or not hasattr(graph, "access_context_block"):
        return ""
    try:
        body = graph.access_context_block(
            target_epoch=str(getattr(self, "_target_epoch", "") or ""),
            required_capabilities=list(
                getattr(self, "requires_capabilities", None) or []),
        )
    except Exception:
        return ""
    return "\n" + body if body.strip() else ""


def _write_board_file(self, wd: "Path") -> bool:
    """Write the FULL board for the post-solve respond path only.

    Solve, explore, verifier, and review paths must not call this method: their
    model context is role-scoped and recorded by ContextManifest.  Respond is an
    operator-requested continuation after solve and may use the complete board.
    The file is written once at run-workspace root, then symlinked into cwd.
    Returns True on success.
    Called from the worker loop at each turn boundary (wd in scope). On ANY
    failure (no graph, permissions, denied file tools) returns False so the
    caller emits the inline fallback instead of a dangling pointer.

    Collision: if a same-named NON-board file exists (a staged attachment or a
    worker scratch file), write to a suffixed name and skip — never clobber it.
    We tag our own files with a sentinel first line to tell them apart."""
    body = self._board_markdown()
    self._board_file_written = False
    if not body:
        return False
    try:
        wd = Path(wd).resolve()
        wd.mkdir(parents=True, exist_ok=True)
        root = workspace_root_for_worker(wd)
        ensure_workspace(root)
        board_path = root / self.BOARD_FILENAME
        path = wd / self.BOARD_FILENAME
        sentinel = "<!-- muteki-team-board -->\n"
        if path.exists():
            head = ""
            try:
                head = path.read_text(errors="ignore")[:64]
            except Exception:
                head = ""
            if sentinel.strip() not in head:
                return False  # a non-board file holds this name — don't clobber
        board_path.write_text(sentinel + body)
        relative_symlink(path, board_path)
        self._board_file_written = True
        return True
    except Exception:
        self._board_file_written = False
        return False


def _remove_unscoped_board_file(self, wd: "Path") -> None:
    """Remove only Muteki-generated full-board artifacts before a solve role runs.

    This also cleans a file left by an older version or a post-solve respond turn
    when an existing Run is resumed. Non-board user files are never touched.
    """
    sentinel = "<!-- muteki-team-board -->"
    worker_path = Path(wd).resolve() / self.BOARD_FILENAME
    root_path = workspace_root_for_worker(wd) / self.BOARD_FILENAME
    for path in (worker_path, root_path):
        try:
            if path.is_symlink():
                try:
                    is_generated = sentinel in path.read_text(
                        errors="ignore")[:128]
                except OSError:
                    is_generated = False
                if is_generated:
                    path.unlink()
                continue
            if path.is_file() and sentinel in path.read_text(
                    errors="ignore")[:128]:
                path.unlink()
        except OSError:
            pass
    self._board_file_written = False


def _target(self) -> "Optional[str]":
    """The live target URL: an operator redirect's _target_override wins over the
    Challenge's original target (the challenge moved / a new host was given)."""
    return getattr(self, "_target_override", None) or self.challenge.target


def _standing_block(self) -> str:
    """Persistent operator guidance (VPS/SSH creds, global constraints) folded
    into every turn's prompt. Empty when none set. Bounded to the most recent
    hints within _STANDING_CHAR_BUDGET chars (defect-4)."""
    sg = getattr(self, "_standing_guidance", None)
    if not sg:
        return ""
    # Exact continuation context is not optional background: reserve its full
    # text first, even when it alone exceeds the ordinary standing budget.  The
    # final invocation fence below will reject the launch if it somehow fails to
    # appear.  All other guidance retains the most-recent-first budget policy.
    required_texts = {
        str(item.get("text") or "")
        for item in (getattr(
            self, "_control_context_prompt_manifest", []) or [])
        if bool(item.get("required")) and item.get("text")
    }
    if required_texts:
        kept_indices = {
            index for index, value in enumerate(sg)
            if value in required_texts
        }
        used = sum(len(sg[index]) + 3 for index in kept_indices)
        for index in range(len(sg) - 1, -1, -1):
            if index in kept_indices:
                continue
            cost = len(sg[index]) + 3
            if used + cost > self._STANDING_CHAR_BUDGET:
                continue
            kept_indices.add(index)
            used += cost
        kept = [value for index, value in enumerate(sg)
                if index in kept_indices]
        body = "\n".join(f"- {s}" for s in kept)
        return ("\n## Operator standing guidance (applies to ALL your work):\n"
                f"{body}\n")

    # keep the most recent hints that fit the char budget (iterate newest-first)
    kept: list[str] = []
    used = 0
    for s in reversed(sg):
        cost = len(s) + 3  # "- " + newline
        if kept and used + cost > self._STANDING_CHAR_BUDGET:
            break
        kept.append(s)
        used += cost
    kept.reverse()
    body = "\n".join(f"- {s}" for s in kept)
    return ("\n## Operator standing guidance (applies to ALL your work):\n"
            f"{body}\n")


def _verifier_rate_limited(self) -> bool:
    return bool(getattr(self.challenge, "verifier_rate_limited", False))


def _verifier_locked_now(self) -> bool:
    """True while a broadcast verifier cooldown/burn-lockout is still in force."""
    return self._verifier_locked_until > time.time()


def _submit_blocked_now(self) -> bool:
    """True while a sibling holds the submit-lock (advisory hold, self-clearing)."""
    return self._submit_blocked_until > time.time()


def _submit_gate_block(self) -> str:
    """Prompt block for a rate-limited-verifier challenge. Empty unless the
    challenge opted in (verifier_rate_limited) — so every ordinary CTF prompt
    is byte-identical. Encodes the submission discipline the burn-lockout
    punishes: self-check offline to the max FIRST, treat each verifier run as a
    scarce shared resource, and HOLD submission while a teammate is submitting
    or the verifier is cooling down."""
    if not self._verifier_rate_limited():
        return ""
    lines = [
        "\n## Verifier submission discipline (this target rate-limits submissions)",
        "The target's scoring verifier punishes wrong/concurrent submissions with "
        "a per-player burn-lockout (a handful of wrong tries → locked out for a "
        "long cooldown, shared across the whole team and across SSH sessions). "
        "Treat each verifier run as an EXPENSIVE, SCARCE, SHARED resource:",
        "1. Before you EVER run the verifier, validate your answer OFFLINE to the "
        "max — build a local checker, cross-check every field/count/format, and "
        "exclude decoys.",
        "2. Acquire the Blackboard Skill submission lock before running the verifier. "
        "Proceed only when the Skill returns WON, run the verifier once, then release "
        "the lock with the result.",
        "3. Do NOT run the verifier to probe. A wrong submission burns the shared "
        "budget for everyone.",
        "4. For any calibration/confidence field, prefer conservative under-claiming. "
        "If the verifier reports a cooldown, stop submitting and use the cooldown to "
        "improve the answer offline.",
    ]
    if self._verifier_locked_now():
        remain = int(self._verifier_locked_until - time.time())
        lines.append(
            f"5. ⚠️ The verifier is CURRENTLY locked (~{remain}s left, a teammate "
            "hit the cooldown). Do NOT submit now. Use this time to perfect your "
            "answer offline so the next single submission lands.")
    elif self._submit_blocked_now():
        lines.append(
            "5. ⚠️ A teammate is submitting RIGHT NOW. Hold your own submission "
            "until their result comes back (it will appear on the board); keep "
            "refining your answer meanwhile, do NOT run the verifier yet.")
    return "\n".join(lines) + "\n"


def _engagement_goal(self) -> str:
    """Render the operator's objective from the versioned domain contract."""
    c = self.challenge
    if getattr(c, "mode", "ctf") == "pentest":
        return str(getattr(getattr(c, "pentest_contract", None), "goal", "") or c.goal)
    return f"Solve {c.name} [{c.category}]"


def _engagement_scope(self) -> str:
    c = self.challenge
    if getattr(c, "mode", "ctf") != "pentest":
        return ""
    if getattr(c, "scope", "") and c.scope.strip():
        return c.scope.strip()
    tgt = self._target()
    if tgt:
        return tgt
    return "Only the target/files provided above. Ask if unsure."


def _box_mode_line(self) -> str:
    staged = list(getattr(self, "_staged_files", None) or [])
    attachments = list(getattr(self.challenge, "attachments", None) or [])
    if staged or attachments:
        return (
            "White-box: attached files in the working directory may be reviewed "
            "as source. The live origin in Scope is still the only system you test."
        )
    return (
        "Black-box: test the live HTTP origin only. Do not search the host "
        "filesystem for challenge source."
    )


def _review_engagement_block(self) -> str:
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return ""
    return (
        "## Engagement goal\n"
        f"{self._engagement_goal()}\n\n"
        "## Scope / authorization\n"
        f"{self._engagement_scope()}\n\n"
        "Review does not accept findings or reports. Stay inside scope. "
        "Reproduction and value judgment are separate from Review.\n\n"
    )



def _live_blackboard_context(self) -> str:
    if self.mode == "review":
        scope = getattr(self, "_review_scope", None) or {}
        if self.shared_graph is not None:
            return self.shared_graph.to_review_projection(
                fact_seqs=scope.get("fact_seqs"),
                since_seq=int(scope.get("since_seq", 0) or 0),
                directive=self.intent_goal,
            )
    if getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        return _ctf_shared_state_block(self)
    parts: list[str] = []
    if self.mode == "bootstrap":
        parts.append(self._dead_ends_scoped_block(epoch_wide=True))
    else:
        parts.append(self._intent_neighborhood_context())
        source_facts, source_meta = self._source_facts_block()
        parts.append(source_facts)
        parts.append(self._step_contract_block())
        parts.append(self._dead_ends_scoped_block())
        artifacts, _artifact_meta = self._source_artifacts_block(source_meta)
        parts.append(artifacts)
    parts.extend((
        _shared_access_block(self),
        self._standing_block(),
        self._team_context_block(),
        self._rejected_flags_block(),
    ))
    return "\n".join(part for part in parts if part and part.strip())


def _build_prompt(self) -> str:
    c = self.challenge
    sections: list[tuple[str, str, dict]] = []
    ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
    tgt = self._target()
    if tgt:
        ctx_lines.append(f"Target: {tgt}")
    if getattr(self, "_staged_files", None):
        ctx_lines.append(
            "Attached files (already in your working directory — inspect them "
            "FIRST): " + ", ".join(self._staged_files))
    sections.append(("header", "\n".join(ctx_lines), {}))
    sections.append((
        "task-instruction", _task_reference_block(self),
        {"external_files": [str(getattr(self, "_task_file_name", ""))]},
    ))
    if not bool(getattr(self, "web_access", True)):
        sections.append(("offline-boundary", _OFFLINE_BOUNDARY_BLOCK, {}))
    if getattr(c, "mode", "ctf") in {"ctf", "pentest"}:
        shared_state = _ctf_shared_state_block(self)
        if shared_state:
            sections.append(("shared-state", shared_state, {}))
        sections.append(("workspace-protocol", self._workspace_protocol_block(), {}))
        toolbox = _toolbox_block(self)
        if toolbox:
            sections.append(("toolbox", toolbox,
                             {"external_files": ["toolbox"]}))
        standing = self._standing_block()
        if standing:
            sections.append(("standing-guidance", standing, {}))
        team = self._team_context_block()
        if team:
            sections.append(("team-context", team, {}))
        rejected = self._rejected_flags_block()
        if rejected:
            sections.append(("rejected-flags", rejected, {}))
        fixed = (
            _PENTEST_FGS_EXPLORE_PROMPT.format(
                ctx="{context}", intent_goal=self.intent_goal or "authorized assessment")
            if getattr(c, "mode", "ctf") == "pentest"
            else _EXEC_PROMPT.format(
                ctx="{context}", kb=_KB_PROMPT if self.kb else "",
                fmt=self._flag_hint())
        )
        return _compile_worker_prompt(self, "worker_bootstrap", fixed, sections)
    # Bootstrap gets no source-facts projection (no assigned intent — the
    # legacy neighborhood/step-contract blocks below still render only when a
    # re-bootstrap carries one). Instead of the old full-board pointer it gets
    # the epoch-wide dead-end projection: every direction THIS epoch ruled out.
    epoch_dead = self._dead_ends_scoped_block(epoch_wide=True)
    if epoch_dead:
        sections.append(("epoch-dead-ends", epoch_dead, {}))
    neighborhood = self._intent_neighborhood_context()
    if neighborhood:
        sections.append(("intent-neighborhood", neighborhood,
                         {"intent_ids": _neighborhood_sibling_ids(neighborhood)}))
    contract = self._step_contract_block()
    if contract:
        sections.append(("step-contract", contract, {}))
    sections.append(("workspace-protocol", self._workspace_protocol_block(), {}))
    toolbox = _toolbox_block(self)
    if toolbox:
        sections.append(("toolbox", toolbox,
                         {"external_files": ["toolbox"]}))
    shared_access = _shared_access_block(self)
    if shared_access:
        sections.append(("shared-access", shared_access, {}))
    poc_block = self._poc_prompt_block()
    if poc_block:
        sections.append(("poc-inheritance", poc_block, {}))
    standing = self._standing_block()
    if standing:
        sections.append(("standing-guidance", standing, {}))
    # rate-limited verifier → submission discipline + live submit-lock / cooldown
    # status. Empty for ordinary challenges (byte-identical prompt).
    gate = self._submit_gate_block()
    if gate:
        sections.append(("submit-gate", gate, {}))
    # re-bootstrap: a course-correction direction from Reason. Steer the rush
    # without narrowing it to a single Explore intent. getattr-guarded so a
    # solver built via __new__ in tests stays safe.
    if getattr(self, "intent_goal", ""):
        sections.append(("course-correction",
            "\n## Course correction (the run drifted — focus here):\n"
            f"{self.intent_goal}", {}))
    # defect-2: multi-flag PROGRESS (N/total from the shared graph). Unified into
    # _team_context_block so explore/resume get it too, not just bootstrap.
    team = self._team_context_block()
    if team:
        sections.append(("team-context", team, {}))
    rejected = self._rejected_flags_block()
    if rejected:
        sections.append(("rejected-flags", rejected, {}))
    fixed = _EXEC_PROMPT.format(
        ctx="{context}",
        kb=_KB_PROMPT if self.kb else "",
        fmt=self._flag_hint())
    return _compile_worker_prompt(self, "worker_bootstrap", fixed, sections)


def _expected_flags(self) -> int:
    return max(1, getattr(self.challenge, "expected_flags", 1) or 1)


def _flags_complete_for_worker(self) -> bool:
    """Whether this run has received its configured number of distinct Flags."""
    if bool(getattr(self.challenge, "platform_confirmation_required", False)):
        return False
    if (getattr(self.challenge, "multi_flag", False)
            and getattr(self.challenge, "expected_flags", 1) <= 1):
        return False
    return len(self._known_flags()) >= self._expected_flags()


def _known_flags(self) -> "list[str]":
    """All flags the RUN already holds — union of this worker's accepted set and
    the SHARED graph's flags (defect-0 made the graph the durable source). The
    union closes the staleness window: a sibling that found a flag has it on the
    shared graph even before this worker's _already_found was seeded."""
    known = set(self._already_found)
    sg = getattr(self, "shared_graph", None)
    if sg is not None:
        try:
            known.update(sg.snapshot().flags or [])
        except Exception:
            pass
    return sorted(known)


def _team_context_block(self) -> str:
    """Render run progress without overriding the Worker's scoped contract.

    A bootstrap Worker owns the end-to-end campaign, while Explore and verifier
    Workers own one bounded Step.  They all need current Flag progress, but only
    the bootstrap Worker should receive the run-level "do not stop" directive.
    """
    n = self._expected_flags()
    if getattr(self.challenge, "mode", "ctf") == "pentest":
        contract = getattr(self.challenge, "pentest_contract", None)
        if contract is None:
            return ""
        graph = getattr(self, "shared_graph", None)
        if graph is None:
            return f"\n## Objective\n{contract.goal}"
        from muteki.pentest.judgement import evaluate
        decision = evaluate(graph.events(), contract)
        return (
            f"\n## Objective\n{contract.goal}\n"
            f"Goal decision: {decision['objective_status']}; "
            f"cited evidence Facts: {decision['qualified_findings']}."
        )
    if n <= 1:
        return ""
    got = self._known_flags()
    remaining = max(0, n - len(got))
    block = [f"\n## Team Flag progress: {len(got)}/{n} captured, "
             f"{remaining} remaining."]
    worker_mode = str(getattr(self, "mode", "bootstrap") or "bootstrap")
    if worker_mode == "bootstrap":
        block.append(
            "This root Worker owns the end-to-end campaign. Keep going after each "
            "Flag until the team has all of them; submit each with `python3 "
            "\"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<flag>'` the moment real "
            "output reveals it. Stop only when all objectives are complete or an "
            "operator-only blocker is proven."
        )
    elif worker_mode == "fact_verifier":
        block.append(
            "This verifier owns only its assigned verification Step. Do not hunt "
            "or submit Flags; finish when that Step's stop condition is met."
        )
    else:
        block.append(
            "只负责当前 Step。真实输出出现 Flag 时立即按 Skill 提交；沿同一因果链推进，"
            "达到预期证据，或有限范围已完成且没有未解释观察时，提交交接并结束。"
        )
    if got:
        block.append("Already found by the team (do NOT re-hunt or re-submit "
                     "these — find the remaining " + str(remaining) + "):")
        block += [f"  - {f}" for f in got]
    return "\n".join(block)


def _rejected_flags_block(self) -> str:
    """run-75379: known-BAD flag values the operator already rejected as false
    positives. A reopened/fresh worker re-runs the producing intent from the
    verified facts, so without this it cheerfully re-derives the SAME bad value
    and re-submits it (the gate then drops it, but the worker wastes the turn and
    the operator sees churn). Telling the model the value is a confirmed dead end
    stops the re-derivation at the source. Rendered for single- AND multi-flag
    runs (a false positive happens in both); empty when nothing was rejected, so
    the prompt is byte-identical on the common path."""
    bad = sorted(self._rejected_flags())
    if not bad:
        return ""
    block = ["\n## Known-BAD flags (operator marked these FALSE POSITIVES — do "
             "NOT submit or re-derive them):"]
    block += [f"  - {f}" for f in bad]
    block.append("These values are confirmed wrong. If your work leads back to "
                 "one, that path is a dead end — pursue a different lead.")
    return "\n".join(block)


def _flag_hint(self) -> str:
    """The 'what a flag looks like' line for the worker prompt. A token-mode
    challenge (flag is a bare secret — a level password, an extracted value),
    NOT flag{...}, must NOT be told to hunt for `flag{...}`.
    Token mode is selected only by flag_format="token". Multi-flag is just a
    collection mode; it must not imply bare-token flags."""
    fmt = getattr(self.challenge, "flag_format", "") or ""
    hint = (getattr(self.challenge, "flag_format_hint", "") or "").strip()
    if hint and fmt != "token":
        return hint
    if fmt == "token":
        return ("a bare token (for example a level password or extracted "
                "secret); submit the exact value through the Blackboard Skill")
    if not fmt:
        return "平台未提供格式；从真实输出取得候选后原样提交"
    return fmt


def _build_explore_prompt(self) -> str:
    c = self.challenge
    if (getattr(c, "mode", "ctf") in {"ctf", "pentest"}
            and (self.mode != "fact_verifier" or c.mode == "pentest")):
        graph = _ctf_shared_state_block(self)
        assigned = (
            f"【你负责的 step】{self.intent_id_assigned}\n"
            f"{self.intent_goal or 'general exploration'}"
        )
        sections = [
            ("shared-graph", graph, {}),
            ("assigned-step", assigned, {
                "intent_ids": [self.intent_id_assigned]
                if self.intent_id_assigned else [],
            }),
        ]
        contract = self._step_contract_block()
        if contract:
            sections.append(("step-contract", contract, {}))
        standing = self._standing_block()
        if standing:
            sections.append(("standing-guidance", standing, {}))
        toolbox = _toolbox_block(self)
        if toolbox:
            sections.append(("toolbox", toolbox, {
                "external_files": ["toolbox"],
            }))
        inherited = self._poc_prompt_block()
        if inherited:
            sections.append(("inherited-pocs", inherited, {}))
        sections.append(("fact-assertion", _CTF_FACT_ASSERTION_NOTE, {}))
        engine = str(getattr(getattr(self, "driver", None), "name", "") or "").strip().lower()
        ensure_role_contract = getattr(self, "_ensure_role_contract", None)
        if engine and engine != "pi" and callable(ensure_role_contract):
            staged = ensure_role_contract()
            fold = str(getattr(staged, "folded_text", "") or "")
            if getattr(staged, "channel", "") == "user_fold" and fold:
                sections.append((
                    "role-contract",
                    fold,
                    {"user_fold_fallback": True},
                ))
        self._persist_context_manifest(
            "worker_explore",
            sections,
            omissions=[],
            full_board_included=True,
        )
        return _join_sections(sections).strip()
    sections: list[tuple[str, str, dict]] = []
    ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
    tgt = self._target()
    if tgt:
        ctx_lines.append(f"Target: {tgt}")
    if getattr(self, "_staged_files", None):
        ctx_lines.append(
            "Attached files (already in your working directory — inspect them "
            "FIRST): " + ", ".join(self._staged_files))
    sections.append(("header", "\n".join(ctx_lines), {}))
    sections.append((
        "task-instruction", _task_reference_block(self),
        {"external_files": [str(getattr(self, "_task_file_name", ""))]},
    ))
    if not bool(getattr(self, "web_access", True)):
        sections.append(("offline-boundary", _OFFLINE_BOUNDARY_BLOCK, {}))
    if getattr(c, "mode", "ctf") == "ctf":
        shared_state = _ctf_shared_state_block(self)
        if shared_state:
            sections.append(("shared-state", shared_state, {}))
        sections.append(("workspace-protocol", self._workspace_protocol_block(), {}))
        toolbox = _toolbox_block(self)
        if toolbox:
            sections.append(("toolbox", toolbox,
                             {"external_files": ["toolbox"]}))
        standing = self._standing_block()
        if standing:
            sections.append(("standing-guidance", standing, {}))
        team = self._team_context_block()
        if team:
            sections.append(("team-context", team, {}))
        rejected = self._rejected_flags_block()
        if rejected:
            sections.append(("rejected-flags", rejected, {}))
        if getattr(c, "mode", "ctf") == "pentest":
            assignment = self.intent_goal or "authorized assessment"
            if self.mode == "fact_verifier":
                assignment = "独立验证，不复用候选 Worker 的结论。" + assignment
            fixed = _PENTEST_FGS_EXPLORE_PROMPT.format(
                ctx="{context}", intent_goal=assignment)
        else:
            template = _CTF_FACT_VERIFY_PROMPT if self.mode == "fact_verifier" else _EXPLORE_PROMPT
            fixed = template.format(
                ctx="{context}", kb=_KB_PROMPT if self.kb else "",
                intent_goal=self.intent_goal or "general exploration",
                fmt=self._flag_hint())
        return _compile_worker_prompt(
            self,
            "verifier" if self.mode == "fact_verifier" else "worker_explore",
            fixed,
            sections,
        )
    # Role-scoped projection instead of the old full-board pointer: the causal
    # neighborhood, the FULL text of this intent's source facts, the step
    # contract, and the dead ends intersecting exactly this step's scope.
    neighborhood = self._intent_neighborhood_context()
    if neighborhood:
        sections.append(("intent-neighborhood", neighborhood,
                         {"intent_ids": _neighborhood_sibling_ids(neighborhood)}))
    source_facts, source_meta = self._source_facts_block()
    if source_facts:
        sections.append(("source-facts", source_facts, source_meta))
    contract = self._step_contract_block()
    if contract:
        sections.append(("step-contract", contract, {}))
    dead = self._dead_ends_scoped_block()
    if dead:
        sections.append(("scoped-dead-ends", dead, {}))
    artifacts, artifacts_meta = self._source_artifacts_block(source_meta)
    if artifacts:
        sections.append(("source-artifacts", artifacts, artifacts_meta))
    sections.append(("workspace-protocol", self._workspace_protocol_block(), {}))
    toolbox = _toolbox_block(self)
    if toolbox:
        sections.append(("toolbox", toolbox,
                         {"external_files": ["toolbox"]}))
    shared_access = _shared_access_block(self)
    if shared_access:
        sections.append(("shared-access", shared_access, {}))
    poc_block = self._poc_prompt_block()
    if poc_block:
        sections.append(("poc-inheritance", poc_block, {}))
    standing = self._standing_block()
    if standing:
        sections.append(("standing-guidance", standing, {}))
    # defect-2: an explore worker must also see N/total flag progress (it didn't)
    # before — only _build_prompt had it), so it doesn't stop after the first.
    team = self._team_context_block()
    if team:
        sections.append(("team-context", team, {}))
    rejected = self._rejected_flags_block()
    if rejected:
        sections.append(("rejected-flags", rejected, {}))
    if self.mode == "fact_verifier":
        template = (
            _CTF_FACT_VERIFY_PROMPT
            if getattr(c, "mode", "ctf") == "ctf"
            else _FACT_VERIFY_PROMPT
        )
        fixed = template.format(
            ctx="{context}",
            kb=_KB_PROMPT if self.kb else "",
            intent_goal=self.intent_goal or "Verify the assigned fact.")
        return _compile_worker_prompt(self, "verifier", fixed, sections)
    fixed = _EXPLORE_PROMPT.format(
        ctx="{context}",
        kb=_KB_PROMPT if self.kb else "",
        intent_goal=self.intent_goal or "general exploration",
        fmt=self._flag_hint())
    return _compile_worker_prompt(self, "worker_explore", fixed, sections)


def _build_review_prompt(self) -> str:
    c = self.challenge
    sections: list[tuple[str, str, dict]] = []
    ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
    tgt = self._target()
    if tgt:
        ctx_lines.append(f"Target: {tgt}")
    sections.append(("header", "\n".join(ctx_lines), {}))
    sections.append((
        "task-instruction", _task_reference_block(self),
        {"external_files": [str(getattr(self, "_task_file_name", ""))]},
    ))
    standing = self._standing_block()
    if standing:
        sections.append(("standing-guidance", standing, {}))
    # Scoped review projection, NOT the full audit board: only the facts under
    # review (the trigger's fact list, or everything since the previous review)
    # plus the intents/dead-ends intersecting that scope. The review subprocess
    # receives neither the full board file nor the raw graph path.
    scope = getattr(self, "_review_scope", None) or {}
    review_board = ""
    if self.shared_graph is not None:
        try:
            review_board = self.shared_graph.to_review_projection(
                fact_seqs=scope.get("fact_seqs"),
                since_seq=int(scope.get("since_seq", 0) or 0),
                directive=self.intent_goal)
        except Exception:
            review_board = ""
    fixed = _REVIEW_PROMPT.format(
        ctx="{context}",
        engagement=self._review_engagement_block(),
        intent_goal=self.intent_goal or "Audit the current swarm trajectory.",
        review_board=review_board or "(no shared graph available)")
    return _compile_worker_prompt(self, "worker_review", fixed, sections)
