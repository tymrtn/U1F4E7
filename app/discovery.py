import asyncio
import json
from typing import AsyncGenerator, Optional
from xml.etree import ElementTree

import dns.resolver
import httpx

PROBE_TIMEOUT = 3

# Providers where MX base domain != submission server domain
MX_ALIASES = {
    "google.com": ["gmail.com"],
    "outlook.com": ["office365.com"],
    "protection.outlook.com": ["office365.com"],
    "microsoft.com": ["office365.com"],
}


async def discover(email: str) -> dict:
    if "@" not in email:
        return {"error": "Invalid email address"}

    domain = email.split("@", 1)[1].lower()

    # Phase 1: Gather candidates from all sources concurrently
    results = await asyncio.gather(
        _discover_srv(domain),
        _discover_autoconfig(domain),
        _discover_mx(domain),
        return_exceptions=True,
    )

    smtp_candidates = []
    imap_candidates = []
    mx_bases = set()

    for r in results:
        if isinstance(r, Exception):
            continue
        smtp_candidates.extend(r.get("smtp", []))
        imap_candidates.extend(r.get("imap", []))
        mx_bases.update(r.get("mx_bases", []))

    # Phase 1b: If MX revealed a provider domain, try autoconfig for it
    # (e.g., MX points to google.com → try autoconfig for gmail.com)
    alias_domains = set()
    for mx_base in mx_bases:
        alias_domains.add(mx_base)
        for alias in MX_ALIASES.get(mx_base, []):
            alias_domains.add(alias)

    # Remove the user's own domain (already tried)
    alias_domains.discard(domain)

    if alias_domains:
        alias_results = await asyncio.gather(
            *[_discover_autoconfig(d) for d in alias_domains],
            return_exceptions=True,
        )
        for r in alias_results:
            if isinstance(r, Exception):
                continue
            smtp_candidates.extend(r.get("smtp", []))
            imap_candidates.extend(r.get("imap", []))

    # Also generate candidates from alias domains
    for alias in alias_domains:
        for port in [465, 587]:
            smtp_candidates.append((f"smtp.{alias}", port, 2, "mx"))
        imap_candidates.append((f"imap.{alias}", 993, 2, "mx"))

    # Fallback: common hostname patterns for user's domain
    common = _common_candidates(domain)
    smtp_candidates.extend(common["smtp"])
    imap_candidates.extend(common["imap"])

    # Phase 2: Probe all candidates in parallel, return best by priority
    smtp_result, imap_result = await asyncio.gather(
        _probe_best(smtp_candidates),
        _probe_best(imap_candidates),
    )

    response = {"domain": domain}
    if smtp_result:
        response["smtp_host"] = smtp_result[0]
        response["smtp_port"] = smtp_result[1]
        response["smtp_source"] = smtp_result[2]
    if imap_result:
        response["imap_host"] = imap_result[0]
        response["imap_port"] = imap_result[1]
        response["imap_source"] = imap_result[2]

    return response


# ── Candidate sources ──
# Each returns {smtp: [(host, port, priority, source_label)], imap: [...]}
# Lower priority number = preferred

async def _discover_srv(domain: str) -> dict:
    def _lookup():
        result = {"smtp": [], "imap": []}

        for name in [
            f"_submissions._tcp.{domain}",
            f"_submission._tcp.{domain}",
        ]:
            try:
                answers = dns.resolver.resolve(name, "SRV")
                for a in answers:
                    host = str(a.target).rstrip(".")
                    if host and host != ".":
                        result["smtp"].append((host, a.port, 0, "srv"))
            except Exception:
                pass

        try:
            answers = dns.resolver.resolve(f"_imaps._tcp.{domain}", "SRV")
            for a in answers:
                host = str(a.target).rstrip(".")
                if host and host != ".":
                    result["imap"].append((host, a.port, 0, "srv"))
        except Exception:
            pass

        return result

    return await asyncio.to_thread(_lookup)


async def _discover_autoconfig(domain: str) -> dict:
    result = {"smtp": [], "imap": []}

    urls = [
        f"https://autoconfig.{domain}/mail/config-v1.1.xml",
        f"https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml",
        f"https://autoconfig.thunderbird.net/v1.1/{domain}",
    ]

    async with httpx.AsyncClient(timeout=5, follow_redirects=True) as client:
        for url in urls:
            try:
                resp = await client.get(url)
                if resp.status_code == 200 and resp.text.strip():
                    _parse_autoconfig(resp.text, result)
                    if result["smtp"] or result["imap"]:
                        break
            except Exception:
                continue

    return result


def _parse_autoconfig(xml_text: str, result: dict):
    try:
        root = ElementTree.fromstring(xml_text)

        for server in root.iter("outgoingServer"):
            host_el = server.find("hostname")
            port_el = server.find("port")
            if host_el is not None and host_el.text and port_el is not None:
                result["smtp"].append(
                    (host_el.text.strip(), int(port_el.text.strip()), 1, "autoconfig")
                )

        for server in root.iter("incomingServer"):
            if server.get("type") == "imap":
                host_el = server.find("hostname")
                port_el = server.find("port")
                if host_el is not None and host_el.text and port_el is not None:
                    result["imap"].append(
                        (host_el.text.strip(), int(port_el.text.strip()), 1, "autoconfig")
                    )
    except Exception:
        pass


async def _discover_mx(domain: str) -> dict:
    def _lookup():
        result = {"smtp": [], "imap": [], "mx_bases": []}

        try:
            answers = dns.resolver.resolve(domain, "MX")
            for a in answers:
                mx_host = str(a.exchange).rstrip(".").lower()
                parts = mx_host.split(".")
                if len(parts) >= 2:
                    mx_base = ".".join(parts[-2:])
                    result["mx_bases"].append(mx_base)
                    for port in [465, 587]:
                        result["smtp"].append((f"smtp.{mx_base}", port, 2, "mx"))
                    result["imap"].append((f"imap.{mx_base}", 993, 2, "mx"))
                    for port in [465, 587]:
                        result["smtp"].append((f"mail.{mx_base}", port, 2, "mx"))
                    result["imap"].append((f"mail.{mx_base}", 993, 2, "mx"))
        except Exception:
            pass

        return result

    return await asyncio.to_thread(_lookup)


def _common_candidates(domain: str) -> dict:
    result = {"smtp": [], "imap": []}

    for host in [f"smtp.{domain}", f"mail.{domain}"]:
        for port in [465, 587]:
            result["smtp"].append((host, port, 3, "common"))

    for host in [f"imap.{domain}", f"mail.{domain}"]:
        result["imap"].append((host, 993, 3, "common"))

    return result


# ── Probing ──

async def _probe_best(
    candidates: list[tuple[str, int, int, str]],
) -> Optional[tuple[str, int, str]]:
    """Probe all candidates in parallel, return highest-priority that connects."""
    if not candidates:
        return None

    # Deduplicate by (host, port), keeping lowest priority
    seen = {}
    for host, port, prio, source in candidates:
        key = (host, port)
        if key not in seen or prio < seen[key][2]:
            seen[key] = (host, port, prio, source)

    unique = list(seen.values())

    async def probe(host, port, prio, source):
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_connection(host, port),
                timeout=PROBE_TIMEOUT,
            )
            writer.close()
            await writer.wait_closed()
            return (host, port, prio, source)
        except Exception:
            return None

    tasks = [probe(h, p, pr, s) for h, p, pr, s in unique]
    results = await asyncio.gather(*tasks)

    successes = [r for r in results if r is not None]
    if not successes:
        return None

    successes.sort(key=lambda x: x[2])
    best = successes[0]
    return (best[0], best[1], best[3])


# ── SSE streaming discovery ──

def _sse_event(event: str, data: dict) -> str:
    return f"event: {event}\ndata: {json.dumps(data)}\n\n"


async def discover_stream(email: str) -> AsyncGenerator[str, None]:
    if "@" not in email:
        yield _sse_event("complete", {"error": "Invalid email address"})
        return

    domain = email.split("@", 1)[1].lower()

    smtp_candidates = []
    imap_candidates = []
    mx_bases = set()

    # Phase: DNS (SRV + MX)
    yield _sse_event("phase", {"name": "dns", "message": "Querying DNS records..."})
    dns_results = await asyncio.gather(
        _discover_srv(domain),
        _discover_mx(domain),
        return_exceptions=True,
    )
    for r in dns_results:
        if isinstance(r, Exception):
            continue
        smtp_candidates.extend(r.get("smtp", []))
        imap_candidates.extend(r.get("imap", []))
        mx_bases.update(r.get("mx_bases", []))

    # Phase: Autoconfig
    yield _sse_event("phase", {"name": "autoconfig", "message": "Checking autoconfig..."})
    autoconfig_result = await _discover_autoconfig(domain)
    smtp_candidates.extend(autoconfig_result.get("smtp", []))
    imap_candidates.extend(autoconfig_result.get("imap", []))

    # Phase: MX alias expansion
    alias_domains = set()
    for mx_base in mx_bases:
        alias_domains.add(mx_base)
        for alias in MX_ALIASES.get(mx_base, []):
            alias_domains.add(alias)
    alias_domains.discard(domain)

    if alias_domains:
        yield _sse_event("phase", {"name": "aliases", "message": f"Trying provider aliases: {', '.join(alias_domains)}"})
        alias_results = await asyncio.gather(
            *[_discover_autoconfig(d) for d in alias_domains],
            return_exceptions=True,
        )
        for r in alias_results:
            if isinstance(r, Exception):
                continue
            smtp_candidates.extend(r.get("smtp", []))
            imap_candidates.extend(r.get("imap", []))

    for alias in alias_domains:
        for port in [465, 587]:
            smtp_candidates.append((f"smtp.{alias}", port, 2, "mx"))
        imap_candidates.append((f"imap.{alias}", 993, 2, "mx"))

    common = _common_candidates(domain)
    smtp_candidates.extend(common["smtp"])
    imap_candidates.extend(common["imap"])

    # Phase: Probing
    yield _sse_event("phase", {"name": "probing", "message": "Probing mail servers..."})
    smtp_result, imap_result = await asyncio.gather(
        _probe_best(smtp_candidates),
        _probe_best(imap_candidates),
    )

    response = {"domain": domain}
    if smtp_result:
        response["smtp_host"] = smtp_result[0]
        response["smtp_port"] = smtp_result[1]
        response["smtp_source"] = smtp_result[2]
    if imap_result:
        response["imap_host"] = imap_result[0]
        response["imap_port"] = imap_result[1]
        response["imap_source"] = imap_result[2]

    yield _sse_event("complete", response)
