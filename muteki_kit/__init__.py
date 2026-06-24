"""muteki_kit — AI-friendly CTF SDK. Self-documenting functions, typed returns.

Solver-generated code imports from here, e.g.:
    from muteki_kit.web import HTTPClient
    from muteki_kit.misc.encoding import auto_decode
    from muteki_kit.web import jwt
    from muteki_kit.result import Result
"""

from muteki_kit import triage
from muteki_kit.result import Result, peek, save_artifact
from muteki_kit.submit import FlagSubmission, submit_flag

__all__ = ["triage", "Result", "peek", "save_artifact", "submit_flag", "FlagSubmission"]
