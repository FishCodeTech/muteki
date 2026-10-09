"""Sparse UI preferences shared by clients of one service installation."""
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, StrictBool


class PreferenceModel(BaseModel):
    model_config = ConfigDict(extra="forbid")


class PresetAccent(PreferenceModel):
    kind: Literal["preset"]
    id: Literal["azure", "violet", "teal", "ember"]


class CustomAccent(PreferenceModel):
    kind: Literal["custom"]
    hue: float = Field(ge=0, lt=360, allow_inf_nan=False)


class DefaultModel(PreferenceModel):
    credentialId: str = Field(min_length=1, max_length=512)
    modelId: str = Field(min_length=1, max_length=512)


class UiValues(PreferenceModel):
    # Defaults are for validation only. exclude_unset preserves absent vs explicit false/default.
    language: Literal["zh", "en"] = "zh"
    theme: Literal["system", "light", "dark"] = "system"
    accent: Annotated[PresetAccent | CustomAccent, Field(discriminator="kind")] = PresetAccent(kind="preset", id="azure")
    sendKey: Literal["enter", "mod-enter"] = "enter"
    runningSend: Literal["queue", "steer"] = "queue"
    defaultAccessMode: Literal["", "supervised", "auto-accept-edits", "auto", "full-access"] = ""
    diffView: Literal["unified", "split"] = "unified"
    diffWrap: StrictBool = False
    diffCollapseUnchanged: StrictBool = True
    readingFontScale: Literal["sm", "md", "lg"] = "md"
    readingDensity: Literal["comfortable", "compact"] = "comfortable"
    readingContentWidth: Literal["narrow", "default", "wide"] = "default"
    defaultModel: DefaultModel | None = None
    hiddenModels: list[Annotated[str, Field(max_length=2048)]] = Field(default_factory=list, max_length=512)


class UiPreferencesWrite(PreferenceModel):
    version: int = Field(strict=True, ge=0)
    values: UiValues


SETTINGS_FEATURES = {
    **{f"settings.{page}": 1 for page in (
        "appearance", "chat", "notifications", "shortcuts", "agents", "agent-extensions",
        "capabilities", "archives", "import", "extensions", "operations", "access",
    )},
    "ui.preferences": 1,
}
