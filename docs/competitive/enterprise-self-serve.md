# Enterprise without a sales team

_How Envelope goes from solopreneur tool to enterprise deployment on full
self-service. Written 2026-07-26. Companion to the roadmap and campaign docs._

---

## The actual problem

Enterprises are not blocked from buying by the absence of a salesperson. They're
blocked by **artifacts they can't get without one**: a security questionnaire
answered, an MSA signed, a DPA executed, an invoice that matches a PO, a named
escalation path. Every one of those is a document or a feature. None of them
requires a human on a call — they only require that the document exists before
the buyer asks for it.

So the strategy is not "sell to enterprise without salespeople." It's
**manufacture every artifact a salesperson would have produced, publish it, and
price under the threshold where procurement gets involved at all.**

Three levers, in order of impact:

1. **Price under the review threshold.** Most companies have a spend line —
   commonly $5k, sometimes $10k — below which a purchase is a manager approval
   and a credit card, not a procurement cycle. Above it, you need a vendor
   review, a security questionnaire, and legal. That review costs *us* a
   salesperson we don't have, so we stay under the line on purpose.
2. **Delete the reason for the review.** Self-hosted means we never touch
   customer data. No sub-processors. No data-processing agreement strictly
   required. The single longest pole in enterprise procurement — vendor data
   handling — doesn't apply to us. That's not a limitation to apologize for,
   it's the enterprise wedge.
3. **Productize the paperwork.** Publish the MSA, the security pack, the
   pre-filled questionnaire, the architecture doc. A buyer who can download the
   answer never files a ticket that needs a human.

---

## Lever 1 — Price to avoid procurement

Today's ladder ends in "Enterprise — contact us." **"Contact us" is a sales
team.** It's the one line in our pricing that requires a human, and it's placed
exactly where volume starts.

Proposed ladder, every tier published and self-checkout:

| Plan | Price | Mailbox users | Per user/yr | Buying motion |
|---|---:|---:|---:|---|
| Personal | Free | 1, non-commercial | — | Download |
| Team | $240/yr | ≤ 10 | $24.00 | Card |
| Growth | $960/yr | ≤ 25 | $38.40 | Card |
| **Business** | **$2,400/yr** | **≤ 100** | **$24.00** | Card or invoice + PO |
| **Fleet** | **$4,800/yr** | **≤ 500, multi-domain** | **$9.60** | Invoice + PO, self-serve quote |
| OEM / reseller / embedded | Contact | — | — | The only human conversation |

Two deliberate choices here.

**Fleet tops out under $5k.** At 500 mailbox users that's $9.60 per user per
year — cheap enough that nobody convenes a committee. We are not maximizing
ACV; we are minimizing the friction that would require headcount to overcome.
The compensation is zero customer-acquisition cost and near-zero cost to serve.

**Per-user price falls as tiers rise.** Standard, and it makes the upgrade
arithmetic obvious to a buyer doing it alone at 11pm without a rep to explain it.

Supporting mechanics, all self-serve:

- **`envelope license quote --users 120`** — generates an invoice-ready PDF
  quote locally, with our entity details, tax IDs, and payment instructions. The
  buyer forwards it to AP. That's the sales team, in a subcommand.
- **Accept POs and ACH above Business**, not just cards. Enterprises frequently
  *cannot* pay by card. Losing a deal over payment rails is unforced.
- **Honor-system seat counting.** We're self-hosted; we can't meter and we
  shouldn't try. `envelope license status` reports local overage and nags. The
  MSA carries a standard audit clause nobody will ever invoke. Fair pricing plus
  visible nagging gets ~90% compliance in developer tools, and chasing the last
  10% costs more than it recovers.
- **Offline license activation.** A signed license file, no phone-home. Required
  for air-gapped deployments and a differentiator besides — it means the license
  check can't become an outage.

## Lever 2 — Make the security review self-service

A trust page that answers the questions before they're asked. One-time build,
permanent leverage:

| Artifact | Why it kills a human touchpoint |
|---|---|
| Architecture + data-flow doc | Answers "where does our mail go?" — the answer is "nowhere," in a diagram |
| **Pre-filled CAIQ / SIG-lite** | The security questionnaire is the #1 thing that forces a call. Publish the completed form as a download |
| Threat model | Signals maturity faster than any certification |
| SBOM per release + dependency policy | Increasingly a hard procurement gate |
| CVE response SLA + security contact | `security.txt`, a published disclosure policy, a real response window |
| Signed releases + checksums | Already partly there via `dist/install.sh` sha256 verification |
| Third-party pen test report | ~$15–30k once. The single highest-ROI artifact on this list |
| Zero-telemetry statement | "We collect nothing" is only credible if it's testable — document how to verify with a packet capture |

**On SOC 2: don't get it, and explain why in public.** SOC 2 attests to a
vendor's controls over customer data. We hold no customer data — the software
runs on the customer's infrastructure and the mail never reaches us. A SOC 2
report for Envelope would attest to the security of a company that isn't in the
data path. Saying that plainly, on the trust page, with the pen test and the
questionnaire next to it, converts better than $50k/yr of compliance theater —
and it's a genuinely stronger answer than what a hosted competitor can give.

Some buyers will have a checkbox that says SOC 2 regardless of applicability.
Some of those are winnable with the explanation. The rest go to the channel
(below) or we let them go.

## Lever 3 — Publish the paperwork

- **Standard MSA**, published, click-through, with a fixed liability cap tied to
  fees paid. Take-it-or-leave-it works at our price point; nobody bills four
  hours of outside counsel to redline a $2,400 contract.
- **DPA available but explained** — we're not a processor for mail content, and
  the doc should say exactly that rather than pretending to be a hyperscaler.
- **Sub-processor list: none.** Publish the empty list. It's a flex.
- **W-9, entity details, insurance posture, vendor onboarding pack** in one
  downloadable zip. Every one of these is otherwise an email to a human.

---

## Product work this requires

These are the features that gate enterprise self-serve. Nothing here is exotic;
what matters is that each one removes a conversation. Proposed as **Phase 5** in
the roadmap.

| # | Item | Removes |
|---|---|---|
| 5.1 | **SSO via OIDC** for the dashboard (SAML only if forced) | "How do our people log in?" — a hard gate at >50 seats |
| 5.2 | **Org roles / RBAC** (was 4.4) | "Who can approve a send?" |
| 5.3 | **Audit log export** — syslog/JSON to Splunk, Datadog, S3 | "Can we get this into our SIEM?" A security-team hard requirement |
| 5.4 | **Secrets-manager backends** — Vault, AWS Secrets Manager, 1Password | "We don't allow passphrases on disk" |
| 5.5 | **Policy as code** — declarative agent/policy config, GitOps, `envelope policy apply -f` | Managing 200 agents through a CLI one at a time. This is what makes fleets self-serve |
| 5.6 | **Helm chart + Terraform module + Docker image** | "How do we deploy this to our cluster?" |
| 5.7 | **Offline license activation** | Air-gapped environments; removes a support ticket per deployment |
| 5.8 | **`license quote` + PO/ACH invoicing** | The AP department |
| 5.9 | **Trust page + security pack** | The security questionnaire |

5.5 is the one that's easy to underrate. A fleet-scale customer with no rep to
call needs to manage agents declaratively, in version control, reviewed like any
other config. Without it, "enterprise self-serve" means an admin clicking
through a dashboard 200 times and then filing a support ticket.

---

## Land and expand, with nobody driving

The motion has to work unattended:

1. **Individual adopts free tier** for their own mail.
2. **They hit the 2-agent limit** — a real, non-punitive wall that arrives
   exactly when the tool has proven itself.
3. **The multiplayer moment**: they want a teammate to see the approval queue.
   Today that means exposing the dashboard on a tailnet with an identity
   allowlist — already shipped, and the natural expansion trigger. Make it
   *obvious*, not a docs footnote.
4. **They need to get it approved.** This is where deals die without a rep, so
   ship a **champion kit**: a one-page internal justification memo they can
   paste into Slack, the security pack link, the MSA, the cost comparison, and
   the quote PDF. Write the memo *for* them, in their voice, arguing to their
   boss. A salesperson's entire job, as a markdown file.
5. **Renewal is automatic** and the seat-count nag handles expansion.

## The channel is the sales team

Some enterprises will not buy from a self-serve vendor no matter how good the
paperwork is. They want someone accountable on a phone. Don't build that
function — **rent it**:

- **Reseller / MSP tier** with a published margin (25–30%) and self-serve
  partner onboarding. MSPs already deploy tooling into client environments and
  they already have the enterprise relationships and the paper.
- **AI consultancies** building agent systems for enterprises are the natural
  fit: Envelope is a component of their delivery, not a vendor their client
  must onboard.
- **OEM/embedded** for products that need governed mail inside their own
  offering. Highest ACV, and it's the one place a human conversation is worth
  it — maybe a handful per year.

The channel absorbs exactly the deals that self-serve can't close, at a margin
cost far below a salaried rep, and it scales without headcount.

## What we deliberately give up

Being explicit so this isn't rediscovered as a failure later:

- **Six-figure ACVs.** Our ceiling is roughly $5k self-serve plus channel deals.
  That's the trade for zero CAC and no headcount.
- **Fortune-500 logos with mandatory vendor programs.** Some require an MSA
  negotiation, a security team call, and a SOC 2 report. We lose those, or the
  channel takes them.
- **Negotiated pricing.** Published or nothing. The moment we discount by
  request, we've started a sales function.
- **Custom feature commitments.** Roadmap influence is not for sale at these
  prices, and pretending otherwise creates obligations only a human can manage.

## Sequencing

| When | Do | Why first |
|---|---|---|
| **Weeks 1–2** | Trust page, pre-filled questionnaire, MSA, sub-processor "none", W-9 pack | Pure documentation, unblocks every deal above ~10 seats, costs nothing but writing |
| **Weeks 3–4** | Publish Business + Fleet tiers, `license quote`, PO/ACH | Removes "contact us" — the last human requirement in the funnel |
| **Weeks 5–10** | 5.1 SSO, 5.3 audit export, 5.6 container/Helm | The three hard gates for >50-seat deployments |
| **Quarter 2** | 5.5 policy as code, 5.4 secrets managers, 5.7 offline licensing | Fleet-scale operation without support tickets |
| **When revenue supports it** | Third-party pen test | Highest-ROI artifact, but it costs real money — time it after the tier expansion lands |
| **Ongoing** | Champion kit, reseller program | Compounding |

## The metric that runs this

Keep a log of **every enterprise deal that stalled and the artifact that was
missing.** Not deal count — the missing artifact. Each quarter, build the top
one. That log is the enterprise roadmap, and it's the only reliable substitute
for the market feedback a sales team would otherwise be collecting.
