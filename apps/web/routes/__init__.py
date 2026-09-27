"""HTTP route registration split from server.py."""
from fastapi import FastAPI

from apps.web.routes.control import register as register_control
from apps.web.routes.credentials import register as register_credentials
from apps.web.routes.events import register as register_events
from apps.web.routes.models import register as register_models
from apps.web.routes.runs import register as register_runs
from apps.web.routes.system import register as register_system
from apps.web.routes.workers import register as register_workers

def register_all(app: FastAPI) -> None:
    register_system(app)
    register_runs(app)
    register_events(app)
    register_control(app)
    register_workers(app)
    register_models(app)
    register_credentials(app)
