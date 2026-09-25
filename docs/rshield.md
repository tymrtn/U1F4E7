# rShield

rShield is Envelope's threat engine. It gives every message a verdict (`clean`, `suspicious`, `dangerous` or `unavailable`) with a 0-100 score and the named signals behind it. `envelope threat explain <uid>` prints the arithmetic.

## Local analyzers (on by default)

Six analyzers run on your machine and send nothing anywhere: `auth_results`, `sender`, `links`, `content`, `attachments`, `ledger`. Turn one off with `envelope config set threat.analyzers.<name> false`.

## Optional analyzers (off by default)

Each of these has its own switch. When one is enabled and cannot give an answer, Envelope does not call the message clean.

### ClamAV (clamd)

Streams each attachment (up to 20 MB) to a clamd you run, using clamd's `INSTREAM` command, with a 5 second limit per attachment. A detection adds `malware_detected` (+100), tags the message `threat:malware`, and blocks the attachment download.

Install and start clamd (macOS, Homebrew):

```bash
brew install clamav
cd "$(brew --prefix)/etc/clamav"
cp freshclam.conf.sample freshclam.conf   # then delete the "Example" line
cp clamd.conf.sample clamd.conf           # delete "Example", set LocalSocket
freshclam                                 # download signatures
clamd                                     # start the daemon
```

In `clamd.conf`, set `LocalSocket` to a path such as `$(brew --prefix)/var/run/clamav/clamd.sock` (create the directory first). See the [ClamAV documentation](https://docs.clamav.net/) for the full setup.

Point Envelope at it:

```bash
envelope config set threat.clamd.address unix:/opt/homebrew/var/run/clamav/clamd.sock
# or, for clamd listening on TCP:
envelope config set threat.clamd.address tcp:127.0.0.1:3310
```

By default a clamd error (daemon down, timeout) is recorded as a skipped analyzer and the other analyzers still decide the verdict. To make every clamd error an `unavailable` verdict instead:

```bash
envelope config set threat.clamd.required true
```

What leaves the machine: attachment bytes go to the clamd address you configured. With a Unix socket or `127.0.0.1`, they stay on this machine.

### Domain reputation (Spamhaus DBL)

Asks the [Spamhaus Domain Blocklist](https://www.spamhaus.org/blocklists/domain-blocklist/) over DNS about the sender's domain and the domains of the links in the message (registrable domains only, deduplicated, at most 10 per message). A listed domain adds `domain_blocklisted`: +60 for phishing, malware or botnet domains, +25 for spam domains, +15 for abused redirectors.

```bash
envelope config set threat.reputation.provider spamhaus-dbl
```

Spamhaus refuses DBL queries that arrive through public resolvers such as 1.1.1.1 or 8.8.8.8. It answers with an error code (`127.255.255.254`), which Envelope reports as `unavailable`. If your system resolver is a public one, use a Spamhaus Data Query Service key instead, either in config or in the environment:

```bash
envelope config set threat.reputation.dqs_key <your-key>
# or
export ENVELOPE_REPUTATION_API_KEY=<your-key>
```

With a key, Envelope queries `<domain>.<key>.dbl.dq.spamhaus.net`. Get a key from the [Spamhaus Data Query Service](https://www.spamhaus.com/data-access/free-data-query-service/).

Spamhaus limits free use of its blocklists and has terms for commercial use. Read the [Spamhaus DNSBL usage terms](https://www.spamhaus.org/blocklists/dnsbl-fair-use-policy/) before you enable this; whether your use qualifies is between you and Spamhaus.

Answers are cached for an hour in `threat-reputation-cache.json` in Envelope's data directory (`envelope paths` shows where), so each domain is asked about at most once an hour. A refused or failed query is never cached.

What leaves the machine: domain names, sent as DNS queries through your system resolver to Spamhaus. Full URLs, paths, query strings, addresses and message content are never sent.

## Reporting

`envelope threat report <uid>` creates a draft (it never sends) with the original message attached as `message/rfc822`. It goes to up to three recipients:

1. `threat.report_to` (default `reportphishing@apwg.org`).
2. The abuse contact of the sending domain's registrar, found in its registration data (RDAP). That registrar can take the domain down.
3. When the sender analyzer found that the sender imitates a domain you know, the abuse contact of the imitated domain's registrar, which usually handles brand protection.

If two of these are the same address, it appears once. If an RDAP lookup fails, that recipient is left out, the others stay, and the command names the lookup that failed (`abuse_contact` in `--json` output).

What leaves the machine: the sending domain name and, when there is one, the imitated domain name, sent over HTTPS to IANA's RDAP bootstrap file and then to each domain's registry (and registrar) RDAP servers.

## Audit log

Every outside lookup (Spamhaus or RDAP) is stored as a `lookup_performed` event on the message that caused it, with the payload `{provider, domain, result}`. Cache hits are not lookups and are not logged.
