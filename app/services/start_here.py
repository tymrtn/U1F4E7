# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

from app.services.policy import get_domain_policy, list_address_policies


async def build_start_here_response(account_id: str) -> dict:
    domain_policy = await get_domain_policy(account_id)

    if not domain_policy:
        return {
            "mode": "onboarding",
            "account_id": account_id,
            "instructions": _onboarding_instructions(account_id),
        }

    address_policies = await list_address_policies(account_id)

    # Truncate kb_text if too long
    policy_copy = dict(domain_policy)
    kb_text = policy_copy.get("kb_text") or ""
    if len(kb_text) > 2000:
        policy_copy["kb_text"] = None
        policy_copy["kb_text_truncated"] = True
        policy_copy["kb_text_url"] = f"/accounts/{account_id}/domain-policy"

    return {
        "mode": "operational",
        "account_id": account_id,
        "domain_policy": policy_copy,
        "address_policies": address_policies,
        "instructions": _operational_instructions(account_id),
    }


def _onboarding_instructions(account_id: str) -> str:
    return f"""Welcome to Envelope. This account has no policy configured yet.

## Step 1: Answer these questions to configure your domain policy

1. What is the primary purpose of this email account? (e.g., customer support, sales, personal)
2. What tone should replies use? (formal / friendly / neutral)
3. What style? (brief/direct / detailed / conversational)
4. What are 3-5 core values that should guide responses? (e.g., honesty, efficiency, empathy)
5. Do you have a knowledge base or FAQ you would like included? (paste text or describe)

## Step 2: Set your domain policy

POST /accounts/{account_id}/domain-policy
{{
  "name": "My Email Policy",
  "description": "What this account is for",
  "values": ["honesty", "efficiency"],
  "tone": "friendly",
  "style": "brief",
  "kb_text": "Optional knowledge base text..."
}}

## Step 3: Add address policies for specific senders or patterns

For each important address or pattern (e.g., *@enterprise.com, boss@company.com):

POST /accounts/{account_id}/address-policies
{{
  "pattern": "*@example.com",
  "purpose": "Why these emails matter",
  "reply_instructions": "How to respond",
  "confidence_threshold": 0.7,
  "trash_criteria": "What to discard without action"
}}

## After setup

Call GET /accounts/{account_id}/start-here again to see operational mode with full instructions."""


def _operational_instructions(account_id: str) -> str:
    return f"""This account is configured and ready for agent-driven email handling.

## Matching incoming messages to policies

1. Extract the sender address from the inbound message
2. Check address_policies for an exact match on pattern first
3. If no exact match, try wildcard patterns using fnmatch (e.g., *@domain.com matches user@domain.com)
4. If no pattern matches, use the domain_policy defaults

## Processing a message

1. Read the message content and sender
2. Find the matching policy (see above)
3. Determine confidence (0.0-1.0) in your proposed action
4. If confidence >= policy.confidence_threshold: act directly
5. If confidence < policy.confidence_threshold: create a draft for human review

## Logging every action (required)

POST /actions/log
{{
  "account_id": "{account_id}",
  "action_type": "inbound_route",
  "confidence": 0.85,
  "justification": "Explain why you took this action",
  "action_taken": "Description of what was done",
  "message_id": "optional-imap-uid",
  "draft_id": "optional-draft-id"
}}

action_type must be one of: inbound_route, draft_approve, draft_reject, send_decision, escalate, trash

## Creating a draft for review

POST /accounts/{account_id}/drafts
{{
  "to": "recipient@example.com",
  "subject": "Re: Original Subject",
  "text": "Draft reply text...",
  "created_by": "agent"
}}

## Sending directly (when confidence is high)

POST /send
{{
  "account_id": "{account_id}",
  "to": "recipient@example.com",
  "subject": "Subject",
  "text": "Body"
}}

## kb_text truncation

If kb_text_truncated is true in the domain_policy, fetch the full text from:
GET /accounts/{account_id}/domain-policy"""
