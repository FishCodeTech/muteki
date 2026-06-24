"""Esolang interpreters — Brainfuck (+ detect), the most common CTF esolang.

Runs Brainfuck safely (bounded steps + tape) and prints output for the
provenance gate. Detection helps the model recognize an esolang blob.
"""

from __future__ import annotations

from typing import Optional

from pydantic import BaseModel


class EsoResult(BaseModel):
    ran: bool
    output: str = ""
    notes: str = ""


def looks_like_brainfuck(s: str) -> bool:
    bf = set("><+-.,[]")
    chars = [c for c in s if not c.isspace()]
    if not chars:
        return False
    return sum(1 for c in chars if c in bf) / len(chars) > 0.9


def run_brainfuck(code: str, *, input_data: bytes = b"",
                  max_steps: int = 5_000_000, tape_size: int = 30000) -> EsoResult:
    """Execute Brainfuck with bounds. Returns the program's output."""
    code = [c for c in code if c in "><+-.,[]"]
    # precompute bracket matches
    stack, match = [], {}
    for i, c in enumerate(code):
        if c == "[":
            stack.append(i)
        elif c == "]":
            if not stack:
                return EsoResult(ran=False, notes="unbalanced ] in program")
            j = stack.pop()
            match[i] = j
            match[j] = i
    if stack:
        return EsoResult(ran=False, notes="unbalanced [ in program")

    tape = bytearray(tape_size)
    ptr = ip = steps = 0
    inp = list(input_data)
    out = bytearray()
    while ip < len(code) and steps < max_steps:
        c = code[ip]
        if c == ">":
            ptr = (ptr + 1) % tape_size
        elif c == "<":
            ptr = (ptr - 1) % tape_size
        elif c == "+":
            tape[ptr] = (tape[ptr] + 1) & 0xFF
        elif c == "-":
            tape[ptr] = (tape[ptr] - 1) & 0xFF
        elif c == ".":
            out.append(tape[ptr])
        elif c == ",":
            tape[ptr] = inp.pop(0) if inp else 0
        elif c == "[" and tape[ptr] == 0:
            ip = match[ip]
        elif c == "]" and tape[ptr] != 0:
            ip = match[ip]
        ip += 1
        steps += 1

    text = bytes(out).decode("utf-8", "replace")
    print(f"[brainfuck] output ({steps} steps): {text!r}")
    note = "hit step limit" if steps >= max_steps else ""
    return EsoResult(ran=True, output=text, notes=note)
