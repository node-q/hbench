"""Regenerate assets/litellm/models.json, the model list the viewer serves at `/api/run/models`.

Run with the pinned LiteLLM and its bundled price map, e.g.
  LITELLM_LOCAL_MODEL_COST_MAP=True uv run --with litellm==1.106.2 python scripts/dev/litellm_models.py
assets/litellm/model_prices_and_context_window.json is that release's litellm/model_prices_and_context_window_backup.json.
"""

import json

import litellm

names = {m if "/" in m else f"{p}/{m}" for p, ms in getattr(litellm, "models_by_provider", {}).items() for m in ms}
print(json.dumps(sorted(names), indent=0))
