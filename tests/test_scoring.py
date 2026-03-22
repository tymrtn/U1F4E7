# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest

from app.credentials.store import create_account
from app.services.scoring import (
    DEFAULT_ATTRIBUTE_CATALOG,
    DEFAULT_BASE_SCORE,
    compute_attribute_score,
    get_agent_attribute_list,
    get_attribute_catalog,
    add_custom_attribute,
    delete_custom_attribute,
    upsert_attribute_catalog,
)


# --- compute_attribute_score ---


def _catalog_with_weights():
    """Return catalog entries with 'weight' key (as stored in DB)."""
    return [
        {**a, "weight": a["default_weight"]}
        for a in DEFAULT_ATTRIBUTE_CATALOG
    ]


def test_compute_attribute_score_single_positive():
    catalog = _catalog_with_weights()
    score, applied = compute_attribute_score(["reply"], catalog, 0.80)
    assert score == 0.90
    assert applied == {"reply": 0.10}


def test_compute_attribute_score_multiple_additive():
    catalog = _catalog_with_weights()
    score, applied = compute_attribute_score(
        ["known_contact", "reply", "low_stakes"], catalog, 0.80
    )
    # 0.80 + 0.00 + 0.10 + 0.05 = 0.95
    assert score == 0.95
    assert len(applied) == 3


def test_compute_attribute_score_clamp_high():
    catalog = _catalog_with_weights()
    # same_company (+0.50) + agent_previously_sent (+0.30) = 0.80 + 0.80 = 1.60 → clamped to 1.0
    score, _ = compute_attribute_score(
        ["same_company", "agent_previously_sent"], catalog, 0.80
    )
    assert score == 1.0


def test_compute_attribute_score_clamp_low():
    catalog = _catalog_with_weights()
    # first_contact (-0.20) + high_net_worth (-0.30) + highly_personalized (-0.30)
    # + mission_critical (-0.30) = 0.80 - 1.10 = -0.30 → clamped to 0.0
    score, _ = compute_attribute_score(
        ["first_contact", "high_net_worth", "highly_personalized", "mission_critical"],
        catalog, 0.80,
    )
    assert score == 0.0


def test_compute_attribute_score_unknown_key_ignored():
    catalog = _catalog_with_weights()
    score, applied = compute_attribute_score(
        ["reply", "nonexistent_key"], catalog, 0.80
    )
    assert score == 0.90
    assert "nonexistent_key" not in applied


def test_compute_attribute_score_empty_list_returns_base():
    catalog = _catalog_with_weights()
    score, applied = compute_attribute_score([], catalog, 0.80)
    assert score == 0.80
    assert applied == {}


# --- get_agent_attribute_list ---


def test_get_agent_attribute_list_no_categories():
    catalog = _catalog_with_weights()
    flat = get_agent_attribute_list(catalog)
    for item in flat:
        assert "category" not in item
        assert set(item.keys()) == {"key", "description"}


def test_get_agent_attribute_list_no_weights():
    catalog = _catalog_with_weights()
    flat = get_agent_attribute_list(catalog)
    for item in flat:
        assert "weight" not in item
        assert "default_weight" not in item


# --- Helpers ---

async def _make_account(username: str, name: str = "test") -> str:
    """Create a real account and return its ID."""
    account = await create_account(
        name=name,
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username=username,
        password="testpass",
    )
    return account["id"]


# --- Async DB tests ---


@pytest.mark.asyncio
async def test_attribute_catalog_crud():
    account_id = await _make_account("user@example.com", "catalog-test")
    # First call returns defaults
    domain, base, catalog = await get_attribute_catalog(account_id)
    assert domain == "example.com"
    assert base == DEFAULT_BASE_SCORE
    assert len(catalog) == len(DEFAULT_ATTRIBUTE_CATALOG)

    # Upsert with new base_score and modified weight
    attrs = [{"key": "reply", "weight": 0.20}]
    domain, new_base, new_catalog = await upsert_attribute_catalog(account_id, 0.75, attrs)
    assert domain == "example.com"
    assert new_base == 0.75
    reply_entry = next(a for a in new_catalog if a["key"] == "reply")
    assert reply_entry["weight"] == 0.20


@pytest.mark.asyncio
async def test_custom_attribute_add_delete():
    account_id = await _make_account("user@custom.com", "custom-test")
    result = await add_custom_attribute(
        account_id, key="vip_client", description="VIP client account",
        category="custom", weight=0.25,
    )
    assert result["key"] == "vip_client"
    assert result["is_custom"] == 1

    # Verify it appears in catalog
    _, _, catalog = await get_attribute_catalog(account_id)
    keys = [a["key"] for a in catalog]
    assert "vip_client" in keys

    # Delete succeeds
    deleted = await delete_custom_attribute(account_id, "vip_client")
    assert deleted is True


@pytest.mark.asyncio
async def test_cannot_delete_builtin_attribute():
    account_id = await _make_account("user@builtin.com", "builtin-test")
    # Initialize catalog in DB first
    await upsert_attribute_catalog(
        account_id, DEFAULT_BASE_SCORE,
        [{"key": "reply", "weight": 0.10}],
    )
    deleted = await delete_custom_attribute(account_id, "reply")
    assert deleted is False


# --- Domain-sharing tests ---


@pytest.mark.asyncio
async def test_same_domain_shares_catalog():
    """Two accounts on @example.com share one attribute catalog."""
    id_a = await _make_account("alice@shared.com", "alice")
    id_b = await _make_account("bob@shared.com", "bob")

    # Upsert via account A
    attrs = [{"key": "reply", "weight": 0.50}]
    domain_a, base_a, _ = await upsert_attribute_catalog(id_a, 0.70, attrs)
    assert domain_a == "shared.com"

    # Read via account B — same catalog
    domain_b, base_b, catalog_b = await get_attribute_catalog(id_b)
    assert domain_b == "shared.com"
    assert base_b == 0.70
    reply_b = next(a for a in catalog_b if a["key"] == "reply")
    assert reply_b["weight"] == 0.50


@pytest.mark.asyncio
async def test_different_domains_separate_catalogs():
    """Accounts on different domains have isolated catalogs."""
    id_foo = await _make_account("user@foo.com", "foo-user")
    id_bar = await _make_account("user@bar.com", "bar-user")

    # Upsert different weights per domain
    await upsert_attribute_catalog(id_foo, 0.60, [{"key": "reply", "weight": 0.30}])
    await upsert_attribute_catalog(id_bar, 0.90, [{"key": "reply", "weight": -0.10}])

    _, base_foo, cat_foo = await get_attribute_catalog(id_foo)
    _, base_bar, cat_bar = await get_attribute_catalog(id_bar)

    assert base_foo == 0.60
    assert base_bar == 0.90
    reply_foo = next(a for a in cat_foo if a["key"] == "reply")
    reply_bar = next(a for a in cat_bar if a["key"] == "reply")
    assert reply_foo["weight"] == 0.30
    assert reply_bar["weight"] == -0.10


@pytest.mark.asyncio
async def test_custom_attribute_shared_across_domain():
    """Custom attribute added via account A visible to account B on same domain."""
    id_a = await _make_account("alice@crossdom.com", "cross-a")
    id_b = await _make_account("bob@crossdom.com", "cross-b")

    await add_custom_attribute(
        id_a, key="partner_org", description="Partner organization",
        category="custom", weight=0.15,
    )

    # Account B sees it
    _, _, catalog_b = await get_attribute_catalog(id_b)
    keys_b = [a["key"] for a in catalog_b]
    assert "partner_org" in keys_b
