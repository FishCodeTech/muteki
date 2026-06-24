"""PCAP analysis — TCP-stream reassembly + flag/credential extraction.

Uses scapy (pure-pip) to dissect packets and reassemble TCP streams. tshark is
used (if present) for protocol-aware --export-objects. Recovered data printed
for the provenance gate; large reassemblies overflow to artifacts via the caller.
"""

from __future__ import annotations

import re
import shutil
import subprocess
from collections import defaultdict
from typing import Optional

from pydantic import BaseModel

_FLAG_RE = re.compile(rb"[A-Za-z0-9_]{1,15}\{[^}]{1,200}\}")


class PcapResult(BaseModel):
    found: bool
    method: str = ""
    flag: Optional[str] = None
    streams: int = 0
    notes: str = ""
    sample: str = ""  # short readable head of the most interesting stream


def reassemble_streams(path: str) -> dict[tuple, bytes]:
    """Reassemble TCP payloads keyed by (src,sport,dst,dport). scapy only."""
    from scapy.all import TCP, rdpcap  # lazy import

    flows: dict[tuple, bytes] = defaultdict(bytes)
    for pkt in rdpcap(path):
        if TCP in pkt and bytes(pkt[TCP].payload):
            ip = pkt.payload
            key = (getattr(ip, "src", "?"), pkt[TCP].sport,
                   getattr(ip, "dst", "?"), pkt[TCP].dport)
            flows[key] += bytes(pkt[TCP].payload)
    return dict(flows)


def find_flag(path: str) -> PcapResult:
    """Scan all reassembled TCP streams (and raw bytes) for a flag."""
    # 1. raw byte scan (catches UDP/other too)
    with open(path, "rb") as f:
        raw = f.read()
    m = _FLAG_RE.search(raw)
    if m:
        flag = m.group(0).decode("latin-1", "replace")
        print(f"[pcap:raw] FLAG: {flag}")
        return PcapResult(found=True, method="raw", flag=flag)

    # 2. TCP stream reassembly
    try:
        streams = reassemble_streams(path)
    except Exception as e:
        return PcapResult(found=False, method="reassemble", notes=f"scapy error: {e}")

    best_sample = ""
    for key, data in streams.items():
        mm = _FLAG_RE.search(data)
        if mm:
            flag = mm.group(0).decode("latin-1", "replace")
            print(f"[pcap:stream] {key} FLAG: {flag}")
            return PcapResult(found=True, method="tcp-stream", flag=flag,
                              streams=len(streams))
        if not best_sample and len(data) > 16:
            best_sample = data[:200].decode("latin-1", "replace")

    print(f"[pcap] {len(streams)} TCP streams, no flag. sample: {best_sample[:200]!r}")
    # 3. tshark export-objects hint
    if shutil.which("tshark"):
        print("[pcap] tip: extract files via "
              "`tshark -r FILE --export-objects http,outdir` then scan them")
    return PcapResult(found=False, method="scan", streams=len(streams),
                      notes="no flag in streams; check exported HTTP/SMB objects, "
                            "DNS/ICMP exfil, or USB/keyboard HID",
                      sample=best_sample[:200])


def tshark_fields(path: str, fields: list[str], display_filter: str = "") -> str:
    """Run tshark to pull specific fields (e.g. ['http.request.uri']). Returns
    the raw output for the model to read. Empty string if tshark absent."""
    if shutil.which("tshark") is None:
        return "(tshark not installed)"
    args = ["tshark", "-r", path, "-T", "fields"]
    for fld in fields:
        args += ["-e", fld]
    if display_filter:
        args += ["-Y", display_filter]
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=60)
        return r.stdout
    except (subprocess.SubprocessError, OSError) as e:
        return f"(tshark error: {e})"
