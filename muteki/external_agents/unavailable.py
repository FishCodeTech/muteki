"""Expose an explicitly unavailable configured adapter without breaking others."""

from __future__ import annotations

from typing import Any

from .base import BaseExternalAgentAdapter


class DependencyUnavailableAdapter(BaseExternalAgentAdapter):
    def __init__(self, adapter_id: str, error: Exception, **kwargs: Any) -> None:
        super().__init__(adapter_id, **kwargs)
        self._dependency_type = type(error)
        self._dependency_message = str(error)

    async def probe(self, request):
        raise self._dependency_type(self._dependency_message)

    async def _launch(self, request, plan, bearer_token):
        raise self._dependency_type(self._dependency_message)

    def send(self, session, input):
        return self.unsupported_input_stream(session, input)
