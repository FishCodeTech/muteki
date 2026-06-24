"""muteki_kit forensics track — stego / pcap / carving / metadata.

Artifact-based, macOS-native. magika+Pillow+numpy+scapy (pure-pip) for the
in-process work; binwalk/exiftool/tshark/zsteg subprocessed if installed
(degrade gracefully). Typed Results + provenance-friendly printing.
"""

from muteki_kit.forensics.carving import CarveResult, identify, scan_embedded
from muteki_kit.forensics.metadata import MetaResult, exif
from muteki_kit.forensics.pcap import (
    PcapResult,
    find_flag,
    reassemble_streams,
    tshark_fields,
)
from muteki_kit.forensics.stego import (
    StegoResult,
    lsb_extract,
    scan,
    steghide_extract,
    try_steghide_passwords,
)
from muteki_kit.forensics.vault import (
    VaultResult,
    ansible_vault_view,
    try_ansible_vault_passwords,
)

__all__ = [
    "StegoResult", "lsb_extract", "scan", "steghide_extract", "try_steghide_passwords",
    "PcapResult", "find_flag", "reassemble_streams", "tshark_fields",
    "CarveResult", "identify", "scan_embedded",
    "MetaResult", "exif",
    "VaultResult", "ansible_vault_view", "try_ansible_vault_passwords",
]
