# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import copy
import json
from datetime import datetime, timezone

from app.db import get_db

DEFAULT_SCORING_RUBRIC = {
    "base_score": 0.80,
    "modifiers": {
        "relationship": {
            "first_contact": -0.20,
            "known_contact": 0.00,
            "established_ally": 0.15,
            "internal": 0.10,
        },
        "intent": {
            "reply": 0.10,
            "follow_up": 0.05,
            "introduction": 0.00,
            "pitch": -0.10,
            "proposal": -0.10,
            "informational": 0.05,
            "request": -0.05,
        },
        "stakes": {
            "low": 0.05,
            "medium": 0.00,
            "high": -0.15,
            "mission_critical": -0.30,
        },
        "ask": {
            "true": -0.20,
            "false": 0.00,
        },
        "domain": {
            "internal": 0.10,
            "research": 0.00,
            "business": 0.00,
            "press": -0.20,
            "investment": -0.10,
            "legal": -0.25,
            "personal": 0.05,
        },
        "recipient_context": {
            "peer": 0.00,
            "senior_executive": -0.05,
            "public_figure": -0.15,
            "academic": -0.05,
            "unknown": -0.10,
        },
    },
}

ATTRIBUTION_SCHEMA = {
    "required": {
        "relationship": {
            "type": "enum",
            "values": ["first_contact", "known_contact", "established_ally", "internal"],
            "description": "How well the sender knows the recipient.",
        },
        "intent": {
            "type": "enum",
            "values": ["reply", "follow_up", "introduction", "pitch", "proposal", "informational", "request"],
            "description": "Primary purpose of the email.",
        },
        "stakes": {
            "type": "enum",
            "values": ["low", "medium", "high", "mission_critical"],
            "description": "How consequential the email is if it is wrong or mistimed.",
        },
        "ask": {
            "type": "boolean",
            "description": "Whether the email is making a request of the recipient.",
        },
        "domain": {
            "type": "enum",
            "values": ["internal", "research", "business", "press", "investment", "legal", "personal"],
            "description": "Primary domain or context of the email.",
        },
        "recipient_context": {
            "type": "enum",
            "values": ["peer", "senior_executive", "public_figure", "academic", "unknown"],
            "description": "Who the recipient is in context.",
        },
        "emotional_tone": {
            "type": "enum",
            "values": ["casual", "professional", "formal", "urgent"],
            "description": "Observed tone of the message.",
        },
        "contains_claims": {
            "type": "boolean",
            "description": "Whether the email asserts facts that could be wrong.",
        },
        "references_prior_thread": {
            "type": "boolean",
            "description": "Whether the email is part of an existing conversation.",
        },
    },
    "optional": {
        "topic_tags": {
            "type": "string[]",
            "description": "Free-form topic labels for additional context.",
        },
        "sensitivity_notes": {
            "type": "string",
            "description": "Why the email might be sensitive.",
        },
    },
    "rules": [
        "These tags must be factual descriptions of the email.",
        "Do not estimate confidence, approval likelihood, routing thresholds, or modifier weights.",
    ],
}

_RUBRIC_SHAPE = {
    "relationship": {"first_contact", "known_contact", "established_ally", "internal"},
    "intent": {"reply", "follow_up", "introduction", "pitch", "proposal", "informational", "request"},
    "stakes": {"low", "medium", "high", "mission_critical"},
    "ask": {"true", "false"},
    "domain": {"internal", "research", "business", "press", "investment", "legal", "personal"},
    "recipient_context": {"peer", "senior_executive", "public_figure", "academic", "unknown"},
}


def get_default_scoring_rubric() -> dict:
    return copy.deepcopy(DEFAULT_SCORING_RUBRIC)


def get_attribution_schema() -> dict:
    return copy.deepcopy(ATTRIBUTION_SCHEMA)


def normalize_scoring_rubric(rubric: dict) -> dict:
    if not isinstance(rubric, dict):
        raise ValueError("Rubric must be an object")

    base_score = rubric.get("base_score")
    modifiers = rubric.get("modifiers")
    if not isinstance(base_score, (int, float)):
        raise ValueError("base_score must be a number")
    if not isinstance(modifiers, dict):
        raise ValueError("modifiers must be an object")

    unexpected_categories = sorted(set(modifiers) - set(_RUBRIC_SHAPE))
    if unexpected_categories:
        raise ValueError(
            "Unknown modifier categories: " + ", ".join(unexpected_categories)
        )

    normalized_modifiers: dict[str, dict[str, float]] = {}
    for category, allowed_keys in _RUBRIC_SHAPE.items():
        values = modifiers.get(category)
        if not isinstance(values, dict):
            raise ValueError(f"modifiers.{category} must be an object")

        unexpected_keys = sorted(set(values) - allowed_keys)
        missing_keys = sorted(allowed_keys - set(values))
        if unexpected_keys:
            raise ValueError(
                f"Unknown modifier keys for {category}: " + ", ".join(unexpected_keys)
            )
        if missing_keys:
            raise ValueError(
                f"Missing modifier keys for {category}: " + ", ".join(missing_keys)
            )

        normalized_values: dict[str, float] = {}
        for key in sorted(allowed_keys):
            value = values.get(key)
            if not isinstance(value, (int, float)):
                raise ValueError(f"modifiers.{category}.{key} must be a number")
            normalized_values[key] = float(value)
        normalized_modifiers[category] = normalized_values

    return {
        "base_score": float(base_score),
        "modifiers": normalized_modifiers,
    }


def compute_attribution_score(attribution: dict, rubric: dict) -> tuple[float, dict[str, float]]:
    normalized_rubric = normalize_scoring_rubric(rubric)
    applied: dict[str, float] = {}

    for category, values in normalized_rubric["modifiers"].items():
        if category == "ask":
            selection = "true" if attribution.get("ask") else "false"
        else:
            selection = attribution.get(category)
        if selection is None:
            continue
        modifier_value = values[selection]
        applied[f"{category}.{selection}"] = modifier_value

    total = normalized_rubric["base_score"] + sum(applied.values())
    clamped = min(1.0, max(0.0, total))
    return round(clamped, 4), applied


async def upsert_scoring_rubric(account_id: str, base_score: float, modifiers: dict) -> dict:
    normalized = normalize_scoring_rubric({
        "base_score": base_score,
        "modifiers": modifiers,
    })
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    await db.execute(
        """INSERT OR REPLACE INTO scoring_rubrics
           (account_id, base_score, modifiers, updated_at)
           VALUES (?, ?, ?, ?)""",
        (
            account_id,
            normalized["base_score"],
            json.dumps(normalized["modifiers"]),
            now,
        ),
    )
    await db.commit()
    return await get_scoring_rubric(account_id)


async def get_scoring_rubric(account_id: str) -> dict:
    db = await get_db()
    cursor = await db.execute(
        "SELECT * FROM scoring_rubrics WHERE account_id = ?",
        (account_id,),
    )
    row = await cursor.fetchone()
    if not row:
        default = get_default_scoring_rubric()
        return {
            "account_id": account_id,
            "base_score": default["base_score"],
            "modifiers": default["modifiers"],
            "updated_at": None,
            "is_default": True,
        }

    modifiers = json.loads(row["modifiers"]) if row["modifiers"] else {}
    normalized = normalize_scoring_rubric({
        "base_score": row["base_score"],
        "modifiers": modifiers,
    })
    return {
        "account_id": row["account_id"],
        "base_score": normalized["base_score"],
        "modifiers": normalized["modifiers"],
        "updated_at": row["updated_at"],
        "is_default": False,
    }
