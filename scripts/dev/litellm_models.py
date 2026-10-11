"""Regenerate assets/litellm/models.json: upstream viewer's `/api/run/models` list (`_litellm_model_names`).

Run with the pinned LiteLLM and its bundled price map, e.g.
  LITELLM_LOCAL_MODEL_COST_MAP=True uv run --with litellm==1.93.0 python scripts/dev/litellm_models.py
"""

import json

import litellm

names = {m if "/" in m else f"{p}/{m}" for p, ms in getattr(litellm, "models_by_provider", {}).items() for m in ms}
print(json.dumps(sorted(names), indent=0))
