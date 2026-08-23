// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

mod commands;
mod mcp;

use clap::{ArgGroup, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "envelope",
    version,
    about = "Email mastery for agents. BYO mailbox — give it an email and password, it does the rest.",
    after_help = r#"GETTING STARTED
  Add an account (auto-discovers IMAP/SMTP from the domain):
    envelope accounts add --email you@gmail.com

  Browse your inbox:
    envelope inbox
    envelope inbox --limit 10 --json

  Read a message (does not mark it as read):
    envelope read 42

  Send an email with an attachment:
    envelope send --to someone@example.com --subject "Report" --body "See attached" --attach report.pdf

  Search:
    envelope search "FROM boss@company.com"
    envelope search "SUBJECT invoice" --folder Sent

  Snooze a message until Monday:
    envelope snooze set 42 --until monday --reason follow-up

  Check for due snoozes and return them:
    envelope unsnooze --once

  Open the local dashboard (inbox, compose, reply, snooze, search):
    envelope serve

  List folders with unread counts:
    envelope folders

AGENT WORKFLOWS
  Watch for new mail in real time (IMAP IDLE push):
    envelope watch --json

  Extract a verification code (blocks until code arrives):
    CODE=$(envelope code --wait 60)

  Schedule a send for business hours:
    envelope send --to cto@example.com --subject "Report" --body "..." --at "monday 9am"

  Import contacts from your inbox, then create a rule:
    envelope contacts import --from-inbox
    envelope rule create --name "VIP" --match-contact-tag vip --action flag=\\Flagged

  Use Envelope as an MCP server (Claude Code, Cursor, Zed):
    envelope mcp --config

  Inspect the resolved local state paths:
    envelope paths
    envelope paths --json

  Share dashboard URLs for agent handoffs:
    export ENVELOPE_DASHBOARD_BASE_URL=https://your-node.tailnet.ts.net
    envelope config set dashboard.base_url https://your-node.tailnet.ts.net

  Every command supports --json for machine consumption:
    envelope inbox --json | jq '.[0].subject'
    envelope folders --json | jq '.[] | {name: .folder, unseen}'

PROVIDERS
  Envelope auto-discovers IMAP/SMTP servers via DNS. Tested with:
    Gmail (app password), Outlook.com, Microsoft Workmail,
    Migadu, Fastmail, self-hosted Dovecot, generic IMAP.

MORE HELP
  envelope <command> --help    Show help for a specific command
  envelope serve               Open the web dashboard at http://localhost:3141
  https://github.com/tymrtn/U1F4E7"#
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Credential storage backend: "file" (default) or "keychain"
    #[arg(long, global = true, default_value = "file")]
    credential_store: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage email accounts
    Accounts {
        #[command(subcommand)]
        subcommand: AccountsCmd,
    },

    /// List messages in a folder
    Inbox {
        /// IMAP folder to list
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Maximum messages to return (1..=1000)
        #[arg(long, default_value = "25", value_parser = parse_agent_list_limit)]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Read a single message by UID
    Read {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Search messages
    Search {
        /// IMAP search query
        query: String,
        /// IMAP folder (ignored when --role/--roles is given)
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Maximum results (1..=1000)
        #[arg(long, default_value = "25", value_parser = parse_agent_list_limit)]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Search by folder role instead of literal --folder. Resolves
        /// provider-specific layouts (e.g. INBOX/sent, [Gmail]/Sent Mail) to a
        /// canonical role. Repeatable, or comma-separated. Known roles: inbox,
        /// drafts, sent, trash, spam, archive, starred. Results include the
        /// source folder. Read-only.
        #[arg(long = "role", alias = "roles", value_delimiter = ',', num_args = 1..)]
        roles: Vec<String>,
    },

    /// Send an email
    Send {
        /// Recipient address
        #[arg(long)]
        to: String,
        /// Subject line
        #[arg(long)]
        subject: String,
        /// Plain-text body
        #[arg(long)]
        body: Option<String>,
        /// HTML body
        #[arg(long)]
        html: Option<String>,
        /// Override the From header (sender identity). SMTP auth still uses --account credentials.
        #[arg(long)]
        from: Option<String>,
        /// CC addresses (comma-separated)
        #[arg(long)]
        cc: Option<String>,
        /// BCC addresses (comma-separated)
        #[arg(long)]
        bcc: Option<String>,
        /// Reply-To address
        #[arg(long)]
        reply_to: Option<String>,
        /// File attachment (repeatable — one --attach per file)
        #[arg(long = "attach")]
        attach: Vec<String>,
        /// Required factual risk attribute: a bounded signal Governor uses to assess
        /// this action's stakes and risk (repeatable, one --attr per true signal; e.g.
        /// --attr informational, --attr financial_content). Attributes provide risk
        /// context, not permission, and every send needs at least one. Discover the
        /// full catalog with `envelope governor catalog --json`.
        #[arg(long = "attr")]
        attr: Vec<String>,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Schedule send for a future time (ISO 8601, relative, or natural like "monday 9am")
        #[arg(long)]
        at: Option<String>,
        /// Send safety mode: draft-only, confirm-send, allowlisted-send, or autonomous-send
        #[arg(long, default_value = "autonomous-send")]
        send_mode: String,
        /// Required when --send-mode confirm-send is used
        #[arg(long)]
        confirm_send: bool,
        /// Allowed recipient email address or domain for --send-mode allowlisted-send (repeatable)
        #[arg(long = "allow-recipient")]
        allow_recipients: Vec<String>,
        /// Confirm that a subject beginning with Re: is intentionally a new message without reply threading
        #[arg(long)]
        confirm_new_re_subject: bool,
        /// Override the actual-send cooldown before the outbox sweep may transmit (built-in default 60s; ENVELOPE_SEND_COOLDOWN_SECONDS also overrides)
        #[arg(long)]
        cooldown_seconds: Option<i64>,
        /// Emergency bypass: transmit immediately instead of queueing into the outbox cooldown.
        /// Must be combined with --confirm-send-now.
        #[arg(long)]
        send_now: bool,
        /// Explicit confirmation required to use --send-now (or --cooldown-seconds 0)
        #[arg(long)]
        confirm_send_now: bool,
    },

    /// Move a message to another folder
    Move {
        /// Message UID
        uid: u32,
        /// Destination folder
        #[arg(long)]
        to_folder: String,
        /// Source folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Copy a message to another folder
    Copy {
        /// Message UID
        uid: u32,
        /// Destination folder
        #[arg(long)]
        to_folder: String,
        /// Source folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Delete a message (moves it to Trash; --permanent --confirm deletes forever)
    Delete {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Expunge instead of moving to Trash (irreversible; requires --confirm,
        /// otherwise runs as a dry run)
        #[arg(long)]
        permanent: bool,
        /// Confirm a --permanent delete
        #[arg(long)]
        confirm: bool,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Manage message flags
    Flag {
        #[command(subcommand)]
        subcommand: FlagCmd,
    },

    /// Bulk message operations (move/copy/flag/delete/tag) over many UIDs
    Bulk {
        #[command(subcommand)]
        subcommand: BulkCmd,
    },

    /// List IMAP folders
    Folders {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Copy mail between two configured IMAP accounts
    Migrate {
        #[command(subcommand)]
        subcommand: MigrateCmd,
    },

    /// Stage a mailbox to a local RFC822 archive (export / verify / restore)
    Backup {
        #[command(subcommand)]
        subcommand: BackupCmd,
    },

    /// Collect and verify local evidence bundles from read-only IMAP searches
    Evidence {
        #[command(subcommand)]
        subcommand: EvidenceCmd,
    },

    /// Check DNS posture for BYO email deliverability
    Deliverability {
        #[command(subcommand)]
        subcommand: DeliverabilityCmd,
    },

    /// Manage attachments
    Attachment {
        #[command(subcommand)]
        subcommand: AttachmentCmd,
    },

    /// Manage drafts
    Draft {
        #[command(subcommand)]
        subcommand: DraftCmd,
    },

    /// Governor attribution: discover the catalog agents declare against
    Governor {
        #[command(subcommand)]
        subcommand: GovernorCmd,
    },

    /// Start the localhost dashboard
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "3141")]
        port: u16,

        /// Address to bind. Defaults to loopback. Binding a non-loopback address
        /// (e.g. 0.0.0.0) requires an auth method — set dashboard.auth_token or
        /// dashboard.tailscale_allow first, or the server refuses to start.
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,

        /// Disable background unsnooze and scheduled-send sweeps (desktop diagnostics / read-only shell)
        #[arg(long)]
        no_background_sweeps: bool,
    },

    /// Compose a new email (licensed tier)
    Compose {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Manage license activation
    License {
        #[command(subcommand)]
        subcommand: LicenseCmd,
    },

    /// Manage per-agent identities and policies
    Agent {
        #[command(subcommand)]
        subcommand: AgentCmd,
    },

    /// Show account attributes
    Attributes {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// View action log
    Actions {
        #[command(subcommand)]
        subcommand: ActionsCmd,
    },

    /// View and acknowledge redacted events
    Events {
        #[command(subcommand)]
        subcommand: EventsCmd,
    },

    /// Snooze a message, list snoozed, or unsnooze
    Snooze {
        #[command(subcommand)]
        subcommand: SnoozeCmd,
    },

    /// Check for due snoozes and return them to their original folder
    Unsnooze {
        /// Run a single sweep and exit (for cron / serve ticker)
        #[arg(long)]
        once: bool,
        /// Account ID or email (sweeps all accounts if omitted)
        #[arg(long)]
        account: Option<String>,
    },

    /// Manage scheduled messages
    Scheduled {
        #[command(subcommand)]
        subcommand: ScheduledCmd,
    },

    /// View conversation threads
    Thread {
        #[command(subcommand)]
        subcommand: ThreadCmd,
    },

    /// Manage message tags and scores
    Tag {
        #[command(subcommand)]
        subcommand: TagCmd,
    },

    /// Manage mail rules (match + action)
    Rule {
        #[command(subcommand)]
        subcommand: RuleCmd,
    },

    /// Manage contacts
    Contacts {
        #[command(subcommand)]
        subcommand: ContactsCmd,
    },

    /// Unsubscribe from a mailing list via List-Unsubscribe header
    Unsubscribe {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Actually execute the unsubscribe (default is dry-run)
        #[arg(long)]
        confirm: bool,
        /// Required factual attribute: a bounded label TRUE of the unsubscribe
        /// message (repeatable — one --attr per fact). A `mailto:` unsubscribe is a
        /// real SMTP send and REQUIRES at least one valid key (e.g. --attr
        /// informational); a missing/invalid declaration fails closed before
        /// Governor/SMTP with attributes_required/attributes_invalid. Discover keys
        /// with `envelope governor catalog --json`. HTTPS one-click unsubscribe (no
        /// SMTP) does not use this.
        #[arg(long = "attr")]
        attr: Vec<String>,
    },

    /// Poll for a verification/OTP code from a recent email
    Code {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Filter by sender domain or address (substring match)
        #[arg(long)]
        from: Option<String>,
        /// Filter by subject (substring match)
        #[arg(long)]
        subject: Option<String>,
        /// Seconds to wait before timing out
        #[arg(long, default_value = "120")]
        wait: u64,
    },

    /// Watch a folder for new messages via IMAP IDLE (push notifications)
    Watch {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// IMAP folder to watch
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// POST event JSON to this URL on each new message
        #[arg(long)]
        webhook: Option<String>,
        /// Run mail rules against new messages (not yet implemented)
        #[arg(long)]
        run_rules: bool,
        /// Enqueue route-matched deliveries for each event and run the durable
        /// delivery executor opportunistically (at-least-once webhook push with
        /// HMAC signing, response capture, and exponential-backoff retries).
        #[arg(long)]
        deliver: bool,
    },

    /// Show resolved local state paths and HOME drift warnings
    Paths,

    /// Diagnose Envelope auth/state health and offer bounded, safe repair
    #[command(
        after_help = "Classifies why mailbox ops can fail even when account metadata reads fine (e.g. credential_decrypt_failed vs decrypted_but_imap_auth_failed). --repair performs an always-safe backup; riskier repairs are reported as not-available. Never prints secrets; never sends email."
    )]
    Doctor {
        /// Account ID or email to diagnose (defaults to the default account)
        #[arg(long)]
        account: Option<String>,
        /// Also attempt a read-only IMAP login probe (no mailbox mutation, no send)
        #[arg(long)]
        check_auth: bool,
        /// Plan/execute bounded repair (backup-before-mutation; safe steps only)
        #[arg(long)]
        repair: bool,
        /// With --repair, only report planned actions; do not mutate state
        #[arg(long)]
        dry_run: bool,
        /// Directory for state backups (defaults to a timestamped dir under app data)
        #[arg(long)]
        backup_dir: Option<String>,
        /// IMAP probe timeout in seconds (1-60)
        #[arg(long, default_value = "15")]
        timeout_secs: u64,
    },

    /// Manage persistent Envelope configuration
    Config {
        #[command(subcommand)]
        subcommand: ConfigCmd,
    },

    /// Verify Envelope HOME, account, IMAP auth, and read-only inbox peek
    #[command(
        after_help = "EXIT CODES\n  0 ok\n  1 paths/internal failure\n  2 account missing or not found\n  3 IMAP auth/connect failure\n  4 inbox peek failure"
    )]
    Quickstart {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// IMAP folder to peek
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Headers-only messages to peek (capped at 25)
        #[arg(long, default_value = "5")]
        peek_limit: u32,
        /// Per-network-phase timeout in seconds (capped at 60)
        #[arg(long, default_value = "15")]
        timeout_secs: u64,
        /// Run only local paths/account phases; no IMAP sockets
        #[arg(long)]
        skip_network: bool,
    },

    /// Show the versioned agent JSON/MCP contract
    Contract {
        /// Limit output to one named surface (for example: inbox, read, send, evidence)
        #[arg(long)]
        surface: Option<String>,
    },

    /// Start the MCP (Model Context Protocol) server over stdio
    Mcp {
        /// Print a ready-to-paste MCP config snippet and exit
        #[arg(long)]
        config: bool,
    },
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// Add a new email account
    Add {
        /// Email address
        #[arg(long)]
        email: String,
        /// Read the mailbox password from stdin. Without this flag, prompts
        /// securely on an interactive terminal.
        #[arg(long)]
        password_stdin: bool,
        /// Account display name
        #[arg(long)]
        name: Option<String>,
        /// SMTP host (auto-discovered if omitted)
        #[arg(long)]
        smtp_host: Option<String>,
        /// SMTP port
        #[arg(long)]
        smtp_port: Option<u16>,
        /// IMAP host (auto-discovered if omitted)
        #[arg(long)]
        imap_host: Option<String>,
        /// IMAP port
        #[arg(long)]
        imap_port: Option<u16>,
        /// Establish the credential store under the INSECURE machine-derived
        /// key (hostname+username) instead of a passphrase. Breaks if the
        /// hostname/username change; only for non-interactive environments
        /// where no passphrase can be provided. Prefer a passphrase.
        #[arg(long)]
        insecure_machine_key: bool,
    },
    /// Re-encrypt the file credential store under a new passphrase
    Rekey,
    /// Discover/import matching macOS Mail.app/Keychain internet-password entries
    ImportKeychain {
        /// Email address to match in Keychain internet-password entries
        #[arg(long)]
        email: String,
        /// Account display name to store if imported
        #[arg(long)]
        name: Option<String>,
        /// IMAP host to search (defaults to imap.<domain>)
        #[arg(long)]
        imap_host: Option<String>,
        /// SMTP host to search (defaults to smtp.<domain>)
        #[arg(long)]
        smtp_host: Option<String>,
        /// IMAP port for verification/storage
        #[arg(long)]
        imap_port: Option<u16>,
        /// SMTP port for verification/storage
        #[arg(long)]
        smtp_port: Option<u16>,
        /// Explicitly permit reading password values from Keychain for auth verification
        #[arg(long)]
        confirm_read: bool,
        /// Store/update Envelope account after both IMAP and SMTP verify
        #[arg(long)]
        import: bool,
    },
    /// List configured accounts
    List,
    /// Print safe (non-secret) mail-client setup settings for an account
    SetupInstructions {
        /// Account ID or email address
        #[arg(long)]
        account: String,
        /// Target mail client (formatting hint only)
        #[arg(long, default_value = "mailapp")]
        client: String,
        /// Also copy the account password to the OS clipboard (never printed)
        #[arg(long)]
        copy_password: bool,
        /// Credential kind to copy with --copy-password: password, imap-password, smtp-password
        #[arg(long, default_value = "auto")]
        kind: String,
        /// Auto-clear the clipboard after N seconds (best-effort)
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Copy an account credential directly to the OS clipboard (never printed)
    CopyPassword {
        /// Account ID or email address
        #[arg(long)]
        account: String,
        /// Credential kind: password, imap-password, smtp-password (required if multiple exist)
        #[arg(long, default_value = "auto")]
        kind: String,
        /// Auto-clear the clipboard after N seconds (best-effort)
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Remove an account
    Remove {
        /// Account ID or email address
        id: String,
    },
    /// View or update an account's outbound signature
    Signature {
        #[command(subcommand)]
        subcommand: SignatureCmd,
    },
}

#[derive(Subcommand)]
enum SignatureCmd {
    /// Show the stored signature(s) for an account
    Show {
        /// Account ID or email address
        #[arg(long)]
        account: String,
    },
    /// Set the plain-text and/or HTML signature for an account
    Set {
        /// Account ID or email address
        #[arg(long)]
        account: String,
        /// Plain-text signature value
        #[arg(long, conflicts_with = "text_file")]
        text: Option<String>,
        /// HTML signature value
        #[arg(long, conflicts_with = "html_file")]
        html: Option<String>,
        /// Read the plain-text signature from a file
        #[arg(long)]
        text_file: Option<String>,
        /// Read the HTML signature from a file
        #[arg(long)]
        html_file: Option<String>,
    },
    /// Clear an account's signature(s); clears both unless --text/--html given
    Clear {
        /// Account ID or email address
        #[arg(long)]
        account: String,
        /// Clear only the plain-text signature
        #[arg(long)]
        text: bool,
        /// Clear only the HTML signature
        #[arg(long)]
        html: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Get a persistent config value
    Get {
        /// Supported key: dashboard.base_url
        key: String,
    },
    /// Set a persistent config value
    Set {
        /// Supported key: dashboard.base_url
        key: String,
        /// Value to store
        value: String,
    },
    /// Unset a persistent config value
    Unset {
        /// Supported key: dashboard.base_url
        key: String,
    },
}

#[derive(Subcommand)]
enum FlagCmd {
    /// Add a flag to a message
    Add {
        /// Message UID
        uid: u32,
        /// Flag name (e.g. \\Seen, \\Flagged)
        flag: String,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove a flag from a message
    Remove {
        /// Message UID
        uid: u32,
        /// Flag name
        flag: String,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum AttachmentCmd {
    /// List attachments for a message
    List {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Download an attachment
    Download {
        /// Message UID
        uid: u32,
        /// Attachment filename
        filename: String,
        /// Output path
        #[arg(long)]
        output: Option<String>,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum GovernorCmd {
    /// Show the vendored Governor attribution catalog (weight-free projection).
    /// Add `--json` for the machine-readable projection used by agents.
    Catalog,
}

#[derive(Subcommand)]
enum DraftCmd {
    /// Create a new draft (IMAP-first: appends to server Drafts folder)
    Create {
        /// Recipient
        #[arg(long)]
        to: String,
        /// Subject
        #[arg(long)]
        subject: Option<String>,
        /// Body text
        #[arg(long)]
        body: Option<String>,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Override the From header (sender identity). SMTP auth still uses --account credentials.
        #[arg(long)]
        from: Option<String>,
        /// CC recipient(s)
        #[arg(long)]
        cc: Option<String>,
        /// BCC recipient(s)
        #[arg(long)]
        bcc: Option<String>,
        /// In-Reply-To Message-ID (for replies)
        #[arg(long)]
        in_reply_to: Option<String>,
        /// Attach a file (repeatable). Bytes are snapshotted into the draft so
        /// review and send preserve the attachment.
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<String>,
        /// Confirm that a subject beginning with Re: is intentionally a new message without reply threading
        #[arg(long)]
        confirm_new_re_subject: bool,
    },
    /// Create a contextual reply draft from a message (quotes the parent)
    Reply {
        /// Source message UID
        uid: u32,
        /// Source folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Reply to all recipients (excludes self from Cc)
        #[arg(long)]
        all: bool,
        /// Agent-authored reply body (plain text)
        #[arg(long)]
        body: Option<String>,
        /// Agent-authored reply body (HTML)
        #[arg(long)]
        html: Option<String>,
        /// Append the account signature
        #[arg(long)]
        signature: bool,
        /// Attach a file (repeatable). Bytes are snapshotted into the draft.
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<String>,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Create a contextual forward draft from a message (includes forwarded block)
    Forward {
        /// Source message UID
        uid: u32,
        /// Source folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Recipient(s) for the forward
        #[arg(long)]
        to: Option<String>,
        /// Agent-authored intro body (plain text)
        #[arg(long)]
        body: Option<String>,
        /// Agent-authored intro body (HTML)
        #[arg(long)]
        html: Option<String>,
        /// Append the account signature
        #[arg(long)]
        signature: bool,
        /// Attach a file (repeatable). Bytes are snapshotted into the draft.
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<String>,
        /// Forward the original message's attachments as draft attachments (opt-in)
        #[arg(long = "include-attachments")]
        include_attachments: bool,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Edit a draft's authored body (preserves the quoted/forwarded block)
    Edit {
        /// Draft ID (local UUID) or IMAP Drafts UID (numeric)
        id: String,
        /// Override the From header (sender identity). SMTP auth still uses --account credentials.
        #[arg(long)]
        from: Option<String>,
        /// New authored body (plain text)
        #[arg(long)]
        body: Option<String>,
        /// New authored body (HTML)
        #[arg(long)]
        html: Option<String>,
        /// Override the To recipient(s)
        #[arg(long)]
        to: Option<String>,
        /// Override the Cc recipient(s)
        #[arg(long)]
        cc: Option<String>,
        /// Override the Bcc recipient(s)
        #[arg(long)]
        bcc: Option<String>,
        /// Override the subject
        #[arg(long)]
        subject: Option<String>,
        /// Apply the account signature (omit to preserve prior state)
        #[arg(long)]
        signature: Option<bool>,
        /// Attach a file to the existing draft (repeatable)
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<String>,
        /// Remove a stored attachment by filename (repeatable)
        #[arg(long = "remove-attach", value_name = "FILENAME")]
        remove_attach: Vec<String>,
        /// Remove all stored attachments before adding any new --attach files
        #[arg(long = "clear-attachments")]
        clear_attachments: bool,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Show a draft's metadata and abridged quote/forward preview
    Show {
        /// Draft ID (local UUID)
        id: String,
    },
    /// List drafts (IMAP-first: fetches from server Drafts folder)
    List {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Send a draft by local ID or IMAP UID (fetches content from IMAP)
    Send {
        /// Draft ID (local UUID) or IMAP UID (numeric)
        id: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Required factual attribute: a bounded label TRUE of this draft
        /// (repeatable — one --attr per fact). Discover the catalog with
        /// `envelope governor catalog --json`.
        #[arg(long = "attr")]
        attr: Vec<String>,
        /// Override the actual-send cooldown before the outbox sweep may transmit (built-in default 60s; ENVELOPE_SEND_COOLDOWN_SECONDS also overrides)
        #[arg(long)]
        cooldown_seconds: Option<i64>,
        /// Emergency bypass: transmit immediately instead of queueing into the outbox cooldown.
        /// Must be combined with --confirm-send-now.
        #[arg(long)]
        send_now: bool,
        /// Explicit confirmation required to use --send-now (or --cooldown-seconds 0)
        #[arg(long)]
        confirm_send_now: bool,
    },
    /// Discard a draft by local ID or IMAP UID
    Discard {
        /// Draft ID (local UUID) or IMAP UID (numeric)
        id: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum LicenseCmd {
    /// Activate a license key.
    ///
    /// Key format: env-lic-<suffix> where <suffix> is at least 16 ASCII
    /// alphanumeric or hyphen characters (total key length >= 24).
    /// Stable error code for bad format: license_key_invalid_format.
    /// The full key is never echoed after storage.
    Activate {
        /// Read the license key from stdin. Without this flag, prompts
        /// securely on an interactive terminal.
        #[arg(long)]
        key_stdin: bool,
        /// Legacy positional key input is accepted only to return a redacted
        /// migration error. Never pass a secret on the command line.
        #[arg(hide = true)]
        legacy_key: Option<String>,
    },
    /// Show current license status
    Status,
    /// Deactivate the current license (revert to free tier)
    Deactivate,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Create a new agent identity (prints its bearer token once)
    Create {
        /// Human-readable agent name (unique)
        name: String,
    },
    /// List agent identities (names, prefixes, status; never token hashes)
    List,
    /// Show one agent identity
    Show {
        /// Agent name
        name: String,
    },
    /// Revoke an agent identity (its token stops authorizing immediately)
    Revoke {
        /// Agent name
        name: String,
    },
    /// Manage an agent's authorization policy
    Policy {
        #[command(subcommand)]
        subcommand: AgentPolicyCmd,
    },
}

#[derive(Subcommand)]
enum AgentPolicyCmd {
    /// Set (upsert) an agent's policy fields
    Set {
        /// Agent name
        name: String,
        /// Allowed accounts: '*' or comma-separated ids/emails
        #[arg(long)]
        allow_accounts: Option<String>,
        /// Allowed folders: '*' or comma-separated names
        #[arg(long)]
        allow_folders: Option<String>,
        /// Allowed actions: '*' or comma-separated action names
        #[arg(long)]
        allow_actions: Option<String>,
        /// Send-mode ceiling: draft-only|confirm-send|allowlisted-send|autonomous-send
        #[arg(long)]
        send_mode_ceiling: Option<String>,
        /// Allowed recipients: comma-separated email/@domain patterns
        #[arg(long)]
        allow_recipients: Option<String>,
    },
    /// Show an agent's policy
    Show {
        /// Agent name
        name: String,
    },
}

#[derive(Subcommand)]
enum ActionsCmd {
    /// Tail the action log
    Tail {
        /// Number of entries
        #[arg(long, default_value = "20")]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Filter to actions attributed to this agent (name or id)
        #[arg(long)]
        agent: Option<String>,
    },
    /// Execute a local audit action for an event
    Exec {
        /// Event ID
        #[arg(long)]
        event_id: String,
        /// Actor responsible for the action
        #[arg(long)]
        actor: String,
        #[command(subcommand)]
        subcommand: ActionsExecCmd,
    },
}

#[derive(Subcommand)]
enum ActionsExecCmd {
    /// Record an event as handled locally without mutating the mailbox
    MarkHandled,
}

#[derive(Subcommand)]
enum EventsCmd {
    /// List recent redacted events
    List {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Number of entries
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Mark an event as acknowledged
    Ack {
        /// Event ID
        event_id: String,
        /// Optional actor label for the CLI caller
        #[arg(long)]
        actor: Option<String>,
    },
    /// Manage event-delivery routes (durable webhook push)
    Routes {
        #[command(subcommand)]
        subcommand: EventRoutesCmd,
    },
    /// Inspect and retry durable event deliveries
    Deliveries {
        #[command(subcommand)]
        subcommand: EventDeliveriesCmd,
    },
}

#[derive(Subcommand)]
enum EventRoutesCmd {
    /// Add a webhook delivery route. Prints the signing secret ONCE — store it
    /// now; it is never shown again.
    Add {
        /// Webhook URL to POST matching events to
        #[arg(long)]
        url: String,
        /// Comma-separated event types to match (default: all)
        #[arg(long)]
        event_types: Option<String>,
        /// Account ID or email to scope the route to (default: default account)
        #[arg(long)]
        account: Option<String>,
        /// Priority (lower runs first)
        #[arg(long, default_value = "100")]
        priority: i64,
    },
    /// List event routes (secret is shown as a prefix only)
    List {
        /// Account ID or email (default: default account)
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove an event route by id
    Remove {
        /// Route ID
        route_id: String,
    },
}

#[derive(Subcommand)]
enum EventDeliveriesCmd {
    /// List deliveries, optionally filtered by status
    List {
        /// Filter: pending | dead | delivered | all
        #[arg(long, default_value = "all")]
        status: String,
        /// Number of entries
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Retry a delivery: clear its dead-letter and backoff so the next executor
    /// pass attempts it again
    Retry {
        /// Delivery ID
        delivery_id: String,
    },
}

#[derive(Subcommand)]
enum SnoozeCmd {
    /// Snooze a message — move it to the Snoozed folder with a return time
    Set {
        /// Message UID
        uid: u32,
        /// When to return: ISO 8601 (2026-03-30T09:00), relative (2h/3d/1w),
        /// or natural (tomorrow, monday, "next week")
        #[arg(long)]
        until: String,
        /// Source folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Optional reason: follow-up, waiting-reply, defer, reminder, review
        #[arg(long)]
        reason: Option<String>,
        /// Optional note / annotation
        #[arg(long)]
        note: Option<String>,
        /// Optional recipient grouping (for waiting-reply follow-ups)
        #[arg(long)]
        recipient: Option<String>,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// List snoozed messages
    List {
        /// Account ID or email (shows all accounts if omitted)
        #[arg(long)]
        account: Option<String>,
    },
    /// Check whether snoozed waiting-reply/follow-up threads received replies
    CheckReplies {
        /// Account ID or email (checks all accounts if omitted)
        #[arg(long)]
        account: Option<String>,
    },
    /// Unsnooze a single message immediately (by UID in the original folder)
    Cancel {
        /// Message UID (the original UID at time of snoozing)
        uid: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum ScheduledCmd {
    /// List scheduled messages
    List {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Take a scheduled message back out of the outbox, keeping the draft
    Hold {
        /// Draft ID
        id: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Cancel a scheduled message by discarding the draft (destructive — use
    /// `hold` to unqueue and keep it)
    Cancel {
        /// Draft ID
        id: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum ThreadCmd {
    /// Show the full conversation thread for a message UID
    Show {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// List recent threads
    List {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Maximum threads to return
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Build thread index from IMAP messages (expensive, do periodically)
    Build {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Maximum messages to scan
        #[arg(long, default_value = "200")]
        limit: u32,
        /// Re-read each folder from the start instead of resuming after the
        /// last build. Repairs threading headers on already-indexed messages;
        /// slower, since it refetches up to --limit messages per folder.
        #[arg(long)]
        rebuild: bool,
    },
}

#[derive(Subcommand)]
enum TagCmd {
    /// Set tags and/or scores on a message
    Set {
        /// Message UID
        uid: u32,
        /// Score in key=value format (repeatable, e.g. --score urgent=0.9)
        #[arg(long)]
        score: Vec<String>,
        /// Tag name (repeatable, e.g. --tag newsletter)
        #[arg(long)]
        tag: Vec<String>,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Show all tags and scores for a message
    Show {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// List messages matching a tag or minimum score filter
    List {
        /// Filter by tag name
        #[arg(long)]
        tag: Option<String>,
        /// Minimum score filter in key=value format (repeatable, e.g. --min-score urgent=0.7)
        #[arg(long)]
        min_score: Vec<String>,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum RuleCmd {
    /// Create a new rule
    #[allow(clippy::struct_field_names)]
    Create {
        /// Rule name (unique per account)
        #[arg(long)]
        name: String,
        /// Glob match on sender address (e.g. "*@notifications.github.com")
        #[arg(long)]
        match_from: Option<String>,
        /// Glob match on recipient address
        #[arg(long)]
        match_to: Option<String>,
        /// Glob match on subject
        #[arg(long)]
        match_subject: Option<String>,
        /// Require tag (repeatable)
        #[arg(long)]
        match_tag: Vec<String>,
        /// Score above threshold in key=value format (repeatable, e.g. --match-score-above urgent=0.7)
        #[arg(long)]
        match_score_above: Vec<String>,
        /// Score below threshold in key=value format (repeatable)
        #[arg(long)]
        match_score_below: Vec<String>,
        /// Require sender's contact to have this tag (repeatable)
        #[arg(long)]
        match_contact_tag: Vec<String>,
        /// Action: move=Folder, flag=name, unflag=name, delete, unsubscribe, tag=name, webhook=url
        #[arg(long)]
        action: String,
        /// Priority (lower runs first)
        #[arg(long, default_value = "100")]
        priority: i64,
        /// Stop evaluating further rules after this one fires
        #[arg(long)]
        stop: bool,
        /// Create disabled so a human can review/preview before enabling
        #[arg(long)]
        disabled: bool,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// List all rules
    List {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Dry-run all rules against a single message
    Test {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Preview enabled rules across a folder without mutating the mailbox
    Preview {
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Maximum messages to inspect
        #[arg(long, default_value = "50")]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Batch-apply rules to messages in a folder (requires --confirm)
    Run {
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Maximum messages to process
        #[arg(long, default_value = "50")]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Confirm this mutating mailbox operation
        #[arg(long)]
        confirm: bool,
    },
    /// Enable a rule by name
    Enable {
        /// Rule name
        name: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Disable a rule by name
    Disable {
        /// Rule name
        name: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Delete a rule by name
    Delete {
        /// Rule name
        name: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Export rules as a Sieve script (file output)
    Export {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },
    /// Publish the exported Sieve script to a ManageSieve server (e.g. Migadu)
    PublishSieve {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
        /// Script name on the ManageSieve server
        #[arg(long, default_value = "envelope-rules")]
        script_name: String,
        /// ManageSieve host override (defaults: sieve.migadu.com for Migadu, else IMAP host)
        #[arg(long)]
        host: Option<String>,
        /// ManageSieve port override (default: 4190)
        #[arg(long)]
        port: Option<u16>,
        /// Per-network-phase timeout in seconds (capped at 60)
        #[arg(long, default_value = "20")]
        timeout_secs: u64,
        /// Plan only; render the script and ManageSieve endpoint without uploading
        #[arg(long)]
        dry_run: bool,
        /// Confirm the mutating upload (PUTSCRIPT + SETACTIVE against the live server)
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    /// Read source IMAP folders into a local RFC822 archive directory.
    /// Source mailbox is read-only; export does not mutate flags or delete.
    /// When --out is omitted, archives to <app-data>/archives/<account>-<timestamp>.
    Export {
        /// Account ID or email for the source mailbox
        #[arg(long)]
        account: String,
        /// Destination archive directory (created if missing).
        /// Defaults to <app-data>/archives/<account>-<timestamp> when omitted.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Include folder glob (repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Exclude folder glob (repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Source messages fetched per IMAP batch (1..=500). Each batch
        /// materializes that many full RFC822 bodies in memory before they are
        /// written to disk, so larger values trade memory for fewer round
        /// trips. Keep the conservative default for mailboxes with large
        /// attachments; raise it only for small-message mailboxes.
        #[arg(long = "batch-size", default_value_t = envelope_email_transport::migrate::DEFAULT_BATCH_SIZE)]
        batch_size: u32,
    },
    /// Validate an archive's manifest, file presence, sizes, and SHA-256 checksums.
    /// Pure local operation — does not contact any IMAP server.
    Verify {
        /// Archive directory previously produced by `backup export`
        #[arg(long)]
        from: std::path::PathBuf,
        /// Treat unreferenced ("extra") files in the archive as a hard failure
        #[arg(long)]
        strict: bool,
    },
    /// Append archived messages to a destination IMAP account.
    /// Append-only; no folders or messages are deleted on the destination.
    /// `--dry-run` plans the run without contacting IMAP for APPENDs.
    Restore {
        /// Account ID or email for the destination mailbox
        #[arg(long)]
        account: String,
        /// Archive directory previously produced by `backup export`
        #[arg(long)]
        from: std::path::PathBuf,
        /// Include folder glob (repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Exclude folder glob (repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Folder rename rule "SRC=DST" (repeatable). Common provider
        /// normalizations (Junk E-mail->Junk, Sent Items->Sent,
        /// Deleted Items->Trash) are exposed as `backup::COMMON_PROVIDER_MAPPINGS`
        /// for documentation; restore only rewrites folders the operator passes
        /// explicitly via this flag.
        #[arg(long = "map")]
        map: Vec<String>,
        /// Plan only; verify archive bytes before reporting `would_append`,
        /// then do not append, create folders, or write restore state
        #[arg(long)]
        dry_run: bool,
        /// Number of messages processed per restore batch
        #[arg(long = "batch-size", default_value_t = envelope_email_transport::migrate::DEFAULT_BATCH_SIZE)]
        batch_size: u32,
    },
    /// Audit a destination's restore-state sidecar against the archive manifest
    /// without contacting any IMAP server. Reports pending-without-done rows
    /// classified as planned / unknown_no_message_id / state_not_in_manifest.
    AuditState {
        /// Destination account ID or email whose `.restore-state-<id>.ndjson`
        /// sidecar should be audited.
        #[arg(long)]
        account: String,
        /// Archive directory previously produced by `backup export`.
        #[arg(long)]
        from: std::path::PathBuf,
        /// Include source folder glob (repeatable). Filters the SOURCE folder
        /// name (pre-mapping), matching `backup restore`'s convention.
        #[arg(long = "include")]
        include: Vec<String>,
        /// Exclude source folder glob (repeatable). Wins over --include.
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Folder rename rule "SRC=DST" (repeatable). Used to compute the
        /// destination folder name in the audit output; identical semantics
        /// to `backup restore --map`.
        #[arg(long = "map")]
        map: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EvidenceCmd {
    /// Collect raw RFC822 messages into a local evidence bundle.
    /// Source mailbox is read-only; collection uses EXAMINE and BODY.PEEK[].
    #[command(group(
        ArgGroup::new("evidence_filter")
            .required(true)
            .multiple(true)
            .args([
                "query",
                "from_address",
                "to_address",
                "subject",
                "since",
                "before",
                "body",
                "keyword",
            ])
    ))]
    Collect {
        /// Account ID or email for the source mailbox
        #[arg(long)]
        account: String,
        /// Explicit source IMAP folder
        #[arg(long)]
        folder: String,
        /// Raw IMAP SEARCH query. Use ALL explicitly for broad exports.
        #[arg(long)]
        query: Option<String>,
        /// Include header-linked thread ancestors and descendants
        #[arg(long)]
        include_thread: bool,
        /// Maximum messages to fetch while expanding header-linked threads
        #[arg(
            long,
            default_value_t = envelope_email_transport::evidence::DEFAULT_MAX_THREAD_MESSAGES,
            value_parser = parse_nonzero_usize
        )]
        max_thread_messages: usize,
        /// Destination evidence bundle directory
        #[arg(long)]
        out: std::path::PathBuf,
        /// Structured FROM search term
        #[arg(long)]
        from_address: Option<String>,
        /// Structured TO search term
        #[arg(long)]
        to_address: Option<String>,
        /// Structured SUBJECT search term
        #[arg(long)]
        subject: Option<String>,
        /// Structured SINCE search term, e.g. 1-Jan-2026
        #[arg(long)]
        since: Option<String>,
        /// Structured BEFORE search term, e.g. 1-Feb-2026
        #[arg(long)]
        before: Option<String>,
        /// Structured BODY search term
        #[arg(long)]
        body: Option<String>,
        /// Structured KEYWORD search term (repeatable)
        #[arg(long)]
        keyword: Vec<String>,
    },
    /// Validate a local evidence bundle without contacting IMAP.
    Verify {
        /// Evidence bundle directory
        #[arg(long)]
        from: std::path::PathBuf,
        /// Treat unreferenced extra .eml files as a hard failure
        #[arg(long)]
        strict: bool,
    },
    /// Read-only source-provenance attachment export for legal evidence.
    /// Source mailbox is read-only; export uses EXAMINE and BODY.PEEK[].
    #[command(subcommand)]
    Attachment(EvidenceAttachmentCmd),
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EvidenceAttachmentCmd {
    /// Export raw attachment bytes with full source-email provenance.
    #[command(group(
        ArgGroup::new("evidence_attachment_select")
            .required(true)
            .args(["uid", "query"])
    ))]
    Export {
        /// Account ID or email for the source mailbox
        #[arg(long)]
        account: String,
        /// Source IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Single source message UID (mutually exclusive with --query)
        #[arg(long)]
        uid: Option<u32>,
        /// Exact original attachment filename. In --uid mode, omit to export
        /// ALL attachments of that message.
        #[arg(long)]
        attachment: Option<String>,
        /// Raw IMAP SEARCH query selecting messages to scan for attachments
        /// (mutually exclusive with --uid)
        #[arg(long)]
        query: Option<String>,
        /// Case-insensitive filename glob (`*`/`?`) filtering attachments
        #[arg(long)]
        filename_glob: Option<String>,
        /// Destination directory for exported attachments
        #[arg(long)]
        out: std::path::PathBuf,
        /// Extract plain text from DOCX/text attachments alongside originals
        #[arg(long)]
        extract_text: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DeliverabilityCmd {
    /// Check read-only DNS records for an email domain
    Check {
        /// Domain to inspect
        #[arg(long)]
        domain: String,
    },
}

fn parse_nonzero_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|e| format!("invalid positive integer: {e}"))?;
    if parsed == 0 {
        return Err("must be greater than 0".to_string());
    }
    Ok(parsed)
}

fn parse_agent_list_limit(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|e| format!("--limit invalid integer: {e}"))?;
    if parsed == 0 {
        return Err("--limit must be at least 1".to_string());
    }
    if parsed > commands::contract::MAX_AGENT_LIST_LIMIT {
        return Err(format!(
            "--limit must be at most {} for agent/CLI read-only list/search surfaces",
            commands::contract::MAX_AGENT_LIST_LIMIT
        ));
    }
    Ok(parsed)
}

#[derive(Subcommand, Debug)]
pub enum MigrateCmd {
    /// Show source folders selected for migration
    Folders {
        /// Source account ID or email
        #[arg(long = "from")]
        from: String,
        /// Destination account ID or email
        #[arg(long = "to")]
        to: String,
        /// Include folder glob (repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Exclude folder glob (repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
    },
    /// Copy selected folders/messages from source to destination
    Run {
        /// Source account ID or email
        #[arg(long = "from")]
        from: String,
        /// Destination account ID or email
        #[arg(long = "to")]
        to: String,
        /// Include folder glob (repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Exclude folder glob (repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Plan only; do not append or write migration state
        #[arg(long)]
        dry_run: bool,
        /// Number of source messages fetched and appended per batch
        #[arg(long = "batch-size", default_value_t = envelope_email_transport::migrate::DEFAULT_BATCH_SIZE)]
        batch_size: u32,
    },
}

#[derive(Subcommand)]
enum ContactsCmd {
    /// Add a contact
    Add {
        #[arg(long)]
        email: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// List contacts
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// Show a contact by email
    Show {
        email: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Add a tag to a contact
    Tag {
        email: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove a tag from a contact
    Untag {
        email: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Import contacts from inbox senders
    Import {
        #[arg(long, default_value = "500")]
        limit: u32,
        #[arg(long)]
        account: Option<String>,
    },
}

/// Common arguments shared across all `envelope bulk` subcommands.
#[derive(clap::Args)]
struct BulkCommonArgs {
    /// Explicit UID list or range spec (e.g. `1,2,9:14`). Mutually exclusive with --query.
    #[arg(long)]
    uids: Option<String>,
    /// IMAP search query to resolve UIDs. Mutually exclusive with --uids.
    #[arg(long)]
    query: Option<String>,
    /// IMAP folder to operate on
    #[arg(long, default_value = "INBOX")]
    folder: String,
    /// Plan only — do not mutate the mailbox
    #[arg(long)]
    dry_run: bool,
    /// Account ID or email
    #[arg(long)]
    account: Option<String>,
}

#[derive(Subcommand)]
enum BulkCmd {
    /// Move matched messages to another folder
    Move {
        /// Destination folder
        #[arg(long)]
        to_folder: String,
        #[command(flatten)]
        common: BulkCommonArgs,
    },
    /// Copy matched messages to another folder
    Copy {
        /// Destination folder
        #[arg(long)]
        to_folder: String,
        #[command(flatten)]
        common: BulkCommonArgs,
    },
    /// Add or remove an IMAP flag on matched messages
    Flag {
        /// Flag name (e.g. \\Seen, \\Flagged)
        #[arg(long)]
        flag: String,
        /// Action: add or remove
        #[arg(long, default_value = "add")]
        action: String,
        #[command(flatten)]
        common: BulkCommonArgs,
    },
    /// Delete matched messages (requires --confirm or falls back to --dry-run)
    Delete {
        /// Confirm the destructive delete (omit to run as dry-run)
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: BulkCommonArgs,
    },
    /// Apply a tag to matched messages
    Tag {
        /// Tag name to apply
        #[arg(long)]
        tag: String,
        #[command(flatten)]
        common: BulkCommonArgs,
    },
}

/// Dispatch a `bulk` subcommand: build the op + target, then run the engine.
fn run_bulk(
    subcommand: BulkCmd,
    json: bool,
    backend: envelope_email_store::CredentialBackend,
) -> anyhow::Result<()> {
    use commands::bulk;
    use envelope_email_transport::bulk::BulkOp;

    match subcommand {
        BulkCmd::Move { to_folder, common } => {
            let target = bulk::build_target(common.uids.as_deref(), common.query.as_deref())?;
            bulk::run(
                BulkOp::Move { to_folder },
                target,
                &common.folder,
                common.dry_run,
                common.account.as_deref(),
                json,
                backend,
            )
        }
        BulkCmd::Copy { to_folder, common } => {
            let target = bulk::build_target(common.uids.as_deref(), common.query.as_deref())?;
            bulk::run(
                BulkOp::Copy { to_folder },
                target,
                &common.folder,
                common.dry_run,
                common.account.as_deref(),
                json,
                backend,
            )
        }
        BulkCmd::Flag {
            flag,
            action,
            common,
        } => {
            let target = bulk::build_target(common.uids.as_deref(), common.query.as_deref())?;
            let op = if action == "add" {
                BulkOp::FlagAdd { flag }
            } else {
                BulkOp::FlagRemove { flag }
            };
            bulk::run(
                op,
                target,
                &common.folder,
                common.dry_run,
                common.account.as_deref(),
                json,
                backend,
            )
        }
        BulkCmd::Delete { confirm, common } => {
            let target = bulk::build_target(common.uids.as_deref(), common.query.as_deref())?;
            // Bulk delete requires --confirm; without it (and without --dry-run)
            // fall back to a dry run so nothing is destroyed by accident.
            let dry_run = bulk::delete_effective_dry_run(common.dry_run, confirm);
            if dry_run && !common.dry_run {
                eprintln!(
                    "bulk delete: no --confirm given — running as DRY RUN. Re-run with --confirm to delete."
                );
            }
            bulk::run(
                BulkOp::Delete,
                target,
                &common.folder,
                dry_run,
                common.account.as_deref(),
                json,
                backend,
            )
        }
        BulkCmd::Tag { tag, common } => {
            let target = bulk::build_target(common.uids.as_deref(), common.query.as_deref())?;
            bulk::run(
                BulkOp::Tag { tag },
                target,
                &common.folder,
                common.dry_run,
                common.account.as_deref(),
                json,
                backend,
            )
        }
    }
}

fn main() {
    // Install the rustls crypto provider before any TLS connections are made.
    // Without this, rustls panics with "Could not automatically determine
    // the process-level CryptoProvider" when async-imap or lettre open TLS.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();

    let backend: envelope_email_store::CredentialBackend = match cli.credential_store.parse() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Commands::Accounts { subcommand } => commands::accounts::run(subcommand, cli.json, backend),
        Commands::Inbox {
            folder,
            limit,
            account,
        } => commands::inbox::run(&folder, limit, account.as_deref(), cli.json, backend),
        Commands::Read {
            uid,
            folder,
            account,
        } => commands::read::run(uid, &folder, account.as_deref(), cli.json, backend),
        Commands::Search {
            query,
            folder,
            limit,
            account,
            roles,
        } => commands::search::run(
            &query,
            &folder,
            limit,
            account.as_deref(),
            &roles,
            cli.json,
            backend,
        ),

        Commands::Send {
            to,
            subject,
            body,
            html,
            from,
            cc,
            bcc,
            reply_to,
            attach,
            attr,
            account,
            at,
            send_mode,
            confirm_send,
            allow_recipients,
            confirm_new_re_subject,
            cooldown_seconds,
            send_now,
            confirm_send_now,
        } => commands::send::run(
            &to,
            &subject,
            body.as_deref(),
            html.as_deref(),
            from.as_deref(),
            cc.as_deref(),
            bcc.as_deref(),
            reply_to.as_deref(),
            &attach,
            &attr,
            account.as_deref(),
            cli.json,
            backend,
            at.as_deref(),
            &send_mode,
            confirm_send,
            &allow_recipients,
            confirm_new_re_subject,
            cooldown_seconds,
            send_now,
            confirm_send_now,
        ),

        Commands::Move {
            uid,
            to_folder,
            folder,
            account,
        } => commands::messages::run_move(
            uid,
            &folder,
            &to_folder,
            account.as_deref(),
            cli.json,
            backend,
        ),

        Commands::Copy {
            uid,
            to_folder,
            folder,
            account,
        } => commands::messages::run_copy(
            uid,
            &folder,
            &to_folder,
            account.as_deref(),
            cli.json,
            backend,
        ),

        Commands::Delete {
            uid,
            folder,
            permanent,
            confirm,
            account,
        } => commands::messages::run_delete(
            uid,
            &folder,
            permanent,
            confirm,
            account.as_deref(),
            cli.json,
            backend,
        ),

        Commands::Flag { subcommand } => match subcommand {
            FlagCmd::Add {
                uid,
                flag,
                folder,
                account,
            } => {
                commands::flags::run_add(uid, &flag, &folder, account.as_deref(), cli.json, backend)
            }
            FlagCmd::Remove {
                uid,
                flag,
                folder,
                account,
            } => commands::flags::run_remove(
                uid,
                &flag,
                &folder,
                account.as_deref(),
                cli.json,
                backend,
            ),
        },
        Commands::Folders { account } => {
            commands::folders::run(account.as_deref(), cli.json, backend)
        }
        Commands::Migrate { subcommand } => commands::migrate::run(subcommand, cli.json, backend),
        Commands::Backup { subcommand } => commands::backup::run(subcommand, cli.json, backend),
        Commands::Evidence { subcommand } => commands::evidence::run(subcommand, cli.json, backend),
        Commands::Deliverability { subcommand } => {
            commands::deliverability::run(subcommand, cli.json)
        }
        Commands::Attachment { subcommand } => match subcommand {
            AttachmentCmd::List {
                uid,
                folder,
                account,
            } => {
                commands::attachments::run_list(uid, &folder, account.as_deref(), cli.json, backend)
            }
            AttachmentCmd::Download {
                uid,
                filename,
                output,
                folder,
                account,
            } => commands::attachments::run_download(
                uid,
                &filename,
                output.as_deref(),
                &folder,
                account.as_deref(),
                cli.json,
                backend,
            ),
        },

        Commands::Draft { subcommand } => match subcommand {
            DraftCmd::List { account } => {
                commands::drafts::run_list(account.as_deref(), cli.json, backend)
            }
            DraftCmd::Create {
                to,
                subject,
                body,
                account,
                from,
                cc,
                bcc,
                in_reply_to,
                attach,
                confirm_new_re_subject,
            } => commands::drafts::run_create(
                &to,
                subject.as_deref(),
                body.as_deref(),
                account.as_deref(),
                cli.json,
                backend,
                from.as_deref(),
                cc.as_deref(),
                bcc.as_deref(),
                in_reply_to.as_deref(),
                &attach,
                confirm_new_re_subject,
            ),
            DraftCmd::Reply {
                uid,
                folder,
                all,
                body,
                html,
                signature,
                attach,
                account,
            } => commands::drafts::run_reply(
                uid,
                &folder,
                account.as_deref(),
                cli.json,
                backend,
                all,
                body.as_deref(),
                html.as_deref(),
                signature,
                &attach,
            ),
            DraftCmd::Forward {
                uid,
                folder,
                to,
                body,
                html,
                signature,
                attach,
                include_attachments,
                account,
            } => commands::drafts::run_forward(
                uid,
                &folder,
                account.as_deref(),
                cli.json,
                backend,
                to.as_deref(),
                body.as_deref(),
                html.as_deref(),
                signature,
                &attach,
                include_attachments,
            ),
            DraftCmd::Edit {
                id,
                from,
                body,
                html,
                to,
                cc,
                bcc,
                subject,
                signature,
                attach,
                remove_attach,
                clear_attachments,
                account,
            } => commands::drafts::run_edit(
                &id,
                account.as_deref(),
                cli.json,
                backend,
                from.as_deref(),
                body.as_deref(),
                html.as_deref(),
                to.as_deref(),
                cc.as_deref(),
                bcc.as_deref(),
                subject.as_deref(),
                signature,
                &attach,
                &remove_attach,
                clear_attachments,
            ),
            DraftCmd::Show { id } => commands::drafts::run_show(&id, cli.json),
            DraftCmd::Send {
                id,
                account,
                attr,
                cooldown_seconds,
                send_now,
                confirm_send_now,
            } => commands::drafts::run_send(
                &id,
                account.as_deref(),
                &attr,
                cli.json,
                backend,
                cooldown_seconds,
                send_now,
                confirm_send_now,
            ),
            DraftCmd::Discard { id, account } => {
                commands::drafts::run_discard(&id, cli.json, account.as_deref(), backend)
            }
        },

        Commands::Governor { subcommand } => match subcommand {
            GovernorCmd::Catalog => commands::governor::run_catalog(cli.json),
        },

        Commands::Serve {
            port,
            bind,
            no_background_sweeps,
        } => commands::serve::run(port, bind, no_background_sweeps),
        Commands::Compose { .. } => {
            eprintln!("License required — visit https://envelope-email.dev");
            std::process::exit(1);
        }
        Commands::License { subcommand } => match subcommand {
            LicenseCmd::Activate {
                key_stdin,
                legacy_key,
            } => {
                if legacy_key.is_some() {
                    Err(anyhow::anyhow!(
                        "license keys must not be passed on the command line; use \
                         `envelope license activate --key-stdin` or run the command \
                         interactively for a hidden prompt"
                    ))
                } else {
                    commands::secret_input::read_secret("License key", key_stdin)
                        .and_then(|key| commands::license::run_activate(&key, cli.json))
                }
            }
            LicenseCmd::Status => commands::license::run_status(cli.json),
            LicenseCmd::Deactivate => commands::license::run_deactivate(cli.json),
        },
        Commands::Agent { subcommand } => match subcommand {
            AgentCmd::Create { name } => commands::agent::run_create(&name, cli.json, backend),
            AgentCmd::List => commands::agent::run_list(cli.json, backend),
            AgentCmd::Show { name } => commands::agent::run_show(&name, cli.json, backend),
            AgentCmd::Revoke { name } => commands::agent::run_revoke(&name, cli.json, backend),
            AgentCmd::Policy { subcommand } => match subcommand {
                AgentPolicyCmd::Set {
                    name,
                    allow_accounts,
                    allow_folders,
                    allow_actions,
                    send_mode_ceiling,
                    allow_recipients,
                } => commands::agent::run_policy_set(
                    &name,
                    allow_accounts.as_deref(),
                    allow_folders.as_deref(),
                    allow_actions.as_deref(),
                    send_mode_ceiling.as_deref(),
                    allow_recipients.as_deref(),
                    cli.json,
                    backend,
                ),
                AgentPolicyCmd::Show { name } => {
                    commands::agent::run_policy_show(&name, cli.json, backend)
                }
            },
        },
        Commands::Attributes { .. } => {
            eprintln!("Not yet implemented: attributes");
            std::process::exit(1);
        }
        Commands::Actions { subcommand } => match subcommand {
            ActionsCmd::Tail {
                limit,
                account,
                agent,
            } => commands::actions::run_tail(
                limit,
                account.as_deref(),
                agent.as_deref(),
                cli.json,
                backend,
            ),
            ActionsCmd::Exec {
                event_id,
                actor,
                subcommand,
            } => match subcommand {
                ActionsExecCmd::MarkHandled => {
                    commands::actions::run_exec_mark_handled(&event_id, &actor, cli.json, backend)
                }
            },
        },
        Commands::Events { subcommand } => match subcommand {
            EventsCmd::List { account, limit } => {
                commands::events::run_list(account.as_deref(), limit, cli.json, backend)
            }
            EventsCmd::Ack { event_id, actor } => {
                commands::events::run_ack(&event_id, actor.as_deref(), cli.json, backend)
            }
            EventsCmd::Routes { subcommand } => match subcommand {
                EventRoutesCmd::Add {
                    url,
                    event_types,
                    account,
                    priority,
                } => commands::events::run_route_add(
                    &url,
                    event_types.as_deref(),
                    account.as_deref(),
                    priority,
                    cli.json,
                    backend,
                ),
                EventRoutesCmd::List { account } => {
                    commands::events::run_route_list(account.as_deref(), cli.json, backend)
                }
                EventRoutesCmd::Remove { route_id } => {
                    commands::events::run_route_remove(&route_id, cli.json, backend)
                }
            },
            EventsCmd::Deliveries { subcommand } => match subcommand {
                EventDeliveriesCmd::List { status, limit } => {
                    commands::events::run_delivery_list(&status, limit, cli.json, backend)
                }
                EventDeliveriesCmd::Retry { delivery_id } => {
                    commands::events::run_delivery_retry(&delivery_id, cli.json, backend)
                }
            },
        },

        Commands::Snooze { subcommand } => match subcommand {
            SnoozeCmd::Set {
                uid,
                until,
                folder,
                reason,
                note,
                recipient,
                account,
            } => commands::snooze::run_snooze(
                uid,
                &until,
                &folder,
                account.as_deref(),
                reason.as_deref(),
                note.as_deref(),
                recipient.as_deref(),
                cli.json,
                backend,
            ),
            SnoozeCmd::List { account } => {
                commands::snooze::run_list(account.as_deref(), cli.json, backend)
            }
            SnoozeCmd::CheckReplies { account } => {
                commands::snooze::run_check_replies(account.as_deref(), cli.json, backend)
            }
            SnoozeCmd::Cancel { uid, account } => {
                commands::snooze::run_unsnooze(uid, account.as_deref(), cli.json, backend)
            }
        },

        Commands::Unsnooze { once: _, account } => {
            commands::snooze::run_check(account.as_deref(), cli.json, backend)
        }

        Commands::Scheduled { subcommand } => match subcommand {
            ScheduledCmd::List { account } => {
                commands::scheduled::run_list(account.as_deref(), cli.json, backend)
            }
            ScheduledCmd::Hold { id, account } => {
                commands::scheduled::run_hold(&id, account.as_deref(), cli.json)
            }
            ScheduledCmd::Cancel { id, account } => {
                commands::scheduled::run_cancel(&id, account.as_deref(), cli.json)
            }
        },

        Commands::Thread { subcommand } => match subcommand {
            ThreadCmd::Show {
                uid,
                folder,
                account,
            } => commands::thread::run_show(uid, &folder, account.as_deref(), cli.json, backend),
            ThreadCmd::List { account, limit } => {
                commands::thread::run_list(account.as_deref(), limit, cli.json, backend)
            }
            ThreadCmd::Build {
                account,
                limit,
                rebuild,
            } => commands::thread::run_build(account.as_deref(), limit, rebuild, cli.json, backend),
        },

        Commands::Tag { subcommand } => match subcommand {
            TagCmd::Set {
                uid,
                score,
                tag,
                folder,
                account,
            } => commands::tag::run_set(
                uid,
                &folder,
                &score,
                &tag,
                account.as_deref(),
                cli.json,
                backend,
            ),
            TagCmd::Show {
                uid,
                folder,
                account,
            } => commands::tag::run_show(uid, &folder, account.as_deref(), cli.json, backend),
            TagCmd::List {
                tag,
                min_score,
                account,
            } => commands::tag::run_list(
                tag.as_deref(),
                &min_score,
                account.as_deref(),
                cli.json,
                backend,
            ),
        },

        Commands::Contacts { subcommand } => match subcommand {
            ContactsCmd::Add {
                email,
                name,
                tag,
                notes,
                account,
            } => commands::contacts::run_add(
                &email,
                name.as_deref(),
                &tag,
                notes.as_deref(),
                account.as_deref(),
                cli.json,
                backend,
            ),
            ContactsCmd::List { tag, account } => {
                commands::contacts::run_list(tag.as_deref(), account.as_deref(), cli.json, backend)
            }
            ContactsCmd::Show { email, account } => {
                commands::contacts::run_show(&email, account.as_deref(), cli.json, backend)
            }
            ContactsCmd::Tag {
                email,
                tag,
                account,
            } => commands::contacts::run_tag(&email, &tag, account.as_deref(), cli.json, backend),
            ContactsCmd::Untag {
                email,
                tag,
                account,
            } => commands::contacts::run_untag(&email, &tag, account.as_deref(), cli.json, backend),
            ContactsCmd::Import { limit, account } => {
                commands::contacts::run_import_inbox(limit, account.as_deref(), cli.json, backend)
            }
        },

        Commands::Bulk { subcommand } => run_bulk(subcommand, cli.json, backend),

        Commands::Rule { subcommand } => match subcommand {
            RuleCmd::Create {
                name,
                match_from,
                match_to,
                match_subject,
                match_tag,
                match_score_above,
                match_score_below,
                match_contact_tag,
                action,
                priority,
                stop,
                disabled,
                account,
            } => commands::rule::run_create(
                &name,
                match_from.as_deref(),
                match_to.as_deref(),
                match_subject.as_deref(),
                &match_tag,
                &match_score_above,
                &match_score_below,
                &match_contact_tag,
                &action,
                priority,
                stop,
                !disabled,
                account.as_deref(),
                cli.json,
                backend,
            ),
            RuleCmd::List { account } => {
                commands::rule::run_list(account.as_deref(), cli.json, backend)
            }
            RuleCmd::Test {
                uid,
                folder,
                account,
            } => commands::rule::run_test(uid, &folder, account.as_deref(), cli.json, backend),
            RuleCmd::Preview {
                folder,
                limit,
                account,
            } => commands::rule::run_preview(&folder, account.as_deref(), limit, cli.json, backend),
            RuleCmd::Run {
                folder,
                limit,
                account,
                confirm,
            } => commands::rule::run_apply(
                &folder,
                account.as_deref(),
                limit,
                confirm,
                cli.json,
                backend,
            ),
            RuleCmd::Enable { name, account } => {
                commands::rule::run_enable(&name, account.as_deref(), cli.json, backend)
            }
            RuleCmd::Disable { name, account } => {
                commands::rule::run_disable(&name, account.as_deref(), cli.json, backend)
            }
            RuleCmd::Delete { name, account } => {
                commands::rule::run_delete(&name, account.as_deref(), cli.json, backend)
            }
            RuleCmd::Export { account } => {
                commands::rule::run_export(account.as_deref(), cli.json, backend)
            }
            RuleCmd::PublishSieve {
                account,
                script_name,
                host,
                port,
                timeout_secs,
                dry_run,
                confirm,
            } => commands::rule::run_publish_sieve(
                account.as_deref(),
                &script_name,
                host.as_deref(),
                port,
                timeout_secs,
                dry_run,
                confirm,
                cli.json,
                backend,
            ),
        },

        Commands::Unsubscribe {
            uid,
            folder,
            account,
            confirm,
            attr,
        } => commands::unsubscribe_cmd::run(
            uid,
            &folder,
            account.as_deref(),
            confirm,
            &attr,
            cli.json,
            backend,
        ),

        Commands::Code {
            account,
            from,
            subject,
            wait,
        } => commands::code::run(
            account.as_deref(),
            from.as_deref(),
            subject.as_deref(),
            wait,
            cli.json,
            backend,
        ),

        Commands::Watch {
            account,
            folder,
            webhook,
            run_rules,
            deliver,
        } => commands::watch::run(
            &folder,
            account.as_deref(),
            webhook.as_deref(),
            run_rules,
            deliver,
            cli.json,
            backend,
        ),

        Commands::Paths => commands::paths::run(cli.json, backend),

        Commands::Doctor {
            account,
            check_auth,
            repair,
            dry_run,
            backup_dir,
            timeout_secs,
        } => commands::doctor::run(commands::doctor::DoctorOptions {
            json: cli.json,
            backend,
            account: account.as_deref(),
            check_auth,
            repair,
            dry_run,
            backup_dir: backup_dir.as_deref(),
            timeout_secs,
        }),

        Commands::Config { subcommand } => commands::config::run(subcommand, cli.json),

        Commands::Quickstart {
            account,
            folder,
            peek_limit,
            timeout_secs,
            skip_network,
        } => commands::quickstart::run(
            cli.json,
            account.as_deref(),
            &folder,
            peek_limit,
            timeout_secs,
            skip_network,
            backend,
        ),

        Commands::Contract { surface } => commands::contract::run(surface.as_deref()),

        Commands::Mcp { config } => {
            if config {
                mcp::print_config();
                Ok(())
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime")
                    .block_on(mcp::run(backend))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::Parser;

    #[test]
    fn doctor_parses_as_diagnostic_command() {
        let cli = Cli::try_parse_from(["envelope", "doctor"]).expect("doctor should parse");
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                check_auth: false,
                repair: false,
                dry_run: false,
                ..
            }
        ));
    }

    #[test]
    fn doctor_repair_dry_run_flags_parse() {
        let cli = Cli::try_parse_from([
            "envelope",
            "doctor",
            "--check-auth",
            "--repair",
            "--dry-run",
            "--account",
            "user@example.com",
        ])
        .expect("doctor repair flags should parse");
        match cli.command {
            Commands::Doctor {
                account,
                check_auth,
                repair,
                dry_run,
                ..
            } => {
                assert_eq!(account.as_deref(), Some("user@example.com"));
                assert!(check_auth);
                assert!(repair);
                assert!(dry_run);
            }
            _ => panic!("expected doctor command"),
        }
    }

    #[test]
    fn deliverability_check_command_parses_domain_and_json_flag() {
        let cli = Cli::try_parse_from([
            "envelope",
            "deliverability",
            "check",
            "--domain",
            "example.com",
            "--json",
        ])
        .expect("deliverability check should parse");

        assert!(cli.json);
        match cli.command {
            Commands::Deliverability {
                subcommand: DeliverabilityCmd::Check { domain },
            } => assert_eq!(domain, "example.com"),
            _ => panic!("expected deliverability check command"),
        }
    }

    #[test]
    fn help_lists_paths_command() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("paths"));
        assert!(help.contains("doctor"));
    }

    #[test]
    fn config_command_parses_dashboard_base_url_set() {
        let cli = Cli::try_parse_from([
            "envelope",
            "config",
            "set",
            "dashboard.base_url",
            "https://dash.example.test/",
        ])
        .expect("config command should parse");

        assert!(matches!(
            cli.command,
            Commands::Config {
                subcommand: ConfigCmd::Set {
                    ref key,
                    ref value
                }
            } if key == "dashboard.base_url" && value == "https://dash.example.test/"
        ));
    }

    #[test]
    fn contract_command_parses_and_lists_agent_surfaces() {
        let cli = Cli::try_parse_from(["envelope", "contract", "--surface", "inbox"])
            .expect("contract command should parse");
        assert!(matches!(
            cli.command,
            Commands::Contract {
                surface: Some(ref surface)
            } if surface == "inbox"
        ));

        let contract = commands::contract::agent_contract();
        assert_eq!(contract["schema"], "envelope.agent_contract.v2");
        let surfaces = contract["surfaces"].as_array().expect("surfaces array");
        for required in [
            "inbox", "read", "search", "thread", "draft", "send", "watch", "otp", "rules",
            "evidence",
        ] {
            assert!(
                surfaces.iter().any(|surface| surface["name"] == required),
                "missing contract surface: {required}"
            );
        }
    }

    #[test]
    fn mcp_tools_are_derived_from_agent_contract_schemas() {
        let tools = mcp::tool_list();
        let tool_entries = tools["tools"].as_array().expect("mcp tools array");
        let inbox = tool_entries
            .iter()
            .find(|tool| tool["name"] == "inbox")
            .expect("inbox MCP tool");
        let contract_inbox = commands::contract::surface("inbox").expect("inbox contract surface");
        assert_eq!(inbox["inputSchema"], contract_inbox["input_schema"]);
    }

    #[test]
    fn evidence_collect_parses_mvp_command_shape() {
        let cli = Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "[Gmail]/All Mail",
            "--query",
            r#"FROM "sender@example.com" SUBJECT "contract""#,
            "--include-thread",
            "--max-thread-messages",
            "25",
            "--out",
            "./evidence-bundle",
        ])
        .expect("evidence collect MVP shape should parse");

        match cli.command {
            Commands::Evidence {
                subcommand:
                    EvidenceCmd::Collect {
                        max_thread_messages,
                        ..
                    },
            } => assert_eq!(max_thread_messages, 25),
            _ => panic!("expected evidence collect command"),
        }
    }

    #[test]
    fn evidence_collect_requires_folder_and_out() {
        let missing_folder = match Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--query",
            "ALL",
            "--out",
            "./bundle",
        ]) {
            Ok(_) => panic!("missing --folder should fail"),
            Err(err) => err,
        };
        assert!(
            missing_folder.to_string().contains("--folder"),
            "expected --folder required error, got: {missing_folder}"
        );

        let missing_out = match Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "INBOX",
            "--query",
            "ALL",
        ]) {
            Ok(_) => panic!("missing --out should fail"),
            Err(err) => err,
        };
        assert!(
            missing_out.to_string().contains("--out"),
            "expected --out required error, got: {missing_out}"
        );
    }

    #[test]
    fn evidence_collect_requires_raw_query_or_structured_filter() {
        let err = match Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "INBOX",
            "--out",
            "./bundle",
        ]) {
            Ok(_) => panic!("missing query/filter should fail"),
            Err(err) => err,
        };
        let s = err.to_string();
        assert!(
            s.contains("--query") || s.contains("--from-address") || s.contains("filter"),
            "expected query/filter validation error, got: {s}"
        );
    }

    #[test]
    fn evidence_collect_rejects_whitespace_query_after_clap_parsing() {
        let cli = Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "INBOX",
            "--query",
            "   ",
            "--out",
            "./bundle",
        ])
        .expect("clap group treats present whitespace query as present");

        let Commands::Evidence {
            subcommand: EvidenceCmd::Collect { query, .. },
        } = cli.command
        else {
            panic!("expected evidence collect command");
        };
        let err = envelope_email_transport::evidence::compile_search_query(
            query.as_deref(),
            &envelope_email_transport::evidence::EvidenceQueryFilters::default(),
        )
        .expect_err("runtime query validation should reject whitespace-only --query");

        assert!(err.to_string().contains("--query must not be empty"));
    }

    #[test]
    fn evidence_collect_rejects_zero_max_thread_messages() {
        let err = match Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "INBOX",
            "--query",
            "ALL",
            "--max-thread-messages",
            "0",
            "--out",
            "./bundle",
        ]) {
            Ok(_) => panic!("zero max thread messages should fail clap validation"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("--max-thread-messages"));
    }

    #[test]
    fn evidence_collect_parses_structured_sugar_filters() {
        let cli = Cli::try_parse_from([
            "envelope",
            "evidence",
            "collect",
            "--account",
            "acct-1",
            "--folder",
            "INBOX",
            "--from-address",
            "sender@example.com",
            "--to-address",
            "recipient@example.com",
            "--subject",
            "contract",
            "--since",
            "1-Jan-2026",
            "--before",
            "1-Feb-2026",
            "--body",
            "payment terms",
            "--keyword",
            "Flagged",
            "--out",
            "./bundle",
        ])
        .expect("structured filters should satisfy evidence collect query requirement");

        assert!(matches!(cli.command, Commands::Evidence { .. }));
    }

    #[test]
    fn evidence_verify_parses_from_and_strict() {
        let cli = Cli::try_parse_from([
            "envelope",
            "evidence",
            "verify",
            "--from",
            "./evidence-bundle",
            "--strict",
        ])
        .expect("evidence verify should parse");

        assert!(matches!(cli.command, Commands::Evidence { .. }));
    }

    #[test]
    fn rule_publish_sieve_dry_run_parses() {
        let cli = Cli::try_parse_from([
            "envelope",
            "rule",
            "publish-sieve",
            "--account",
            "acct-1",
            "--script-name",
            "envelope-rules",
            "--dry-run",
        ])
        .expect("publish-sieve dry-run should parse");

        match cli.command {
            Commands::Rule {
                subcommand:
                    RuleCmd::PublishSieve {
                        ref account,
                        ref script_name,
                        host,
                        port,
                        dry_run,
                        confirm,
                        ..
                    },
            } => {
                assert_eq!(account.as_deref(), Some("acct-1"));
                assert_eq!(script_name, "envelope-rules");
                assert!(host.is_none());
                assert!(port.is_none());
                assert!(dry_run);
                assert!(!confirm);
            }
            _ => panic!("expected rule publish-sieve command"),
        }
    }

    #[test]
    fn rule_publish_sieve_confirm_parses_with_overrides() {
        let cli = Cli::try_parse_from([
            "envelope",
            "rule",
            "publish-sieve",
            "--account",
            "acct-1",
            "--host",
            "sieve.alt.example",
            "--port",
            "4191",
            "--timeout-secs",
            "30",
            "--confirm",
        ])
        .expect("publish-sieve confirm should parse");

        match cli.command {
            Commands::Rule {
                subcommand:
                    RuleCmd::PublishSieve {
                        host,
                        port,
                        timeout_secs,
                        confirm,
                        ref script_name,
                        ..
                    },
            } => {
                assert_eq!(host.as_deref(), Some("sieve.alt.example"));
                assert_eq!(port, Some(4191));
                assert_eq!(timeout_secs, 30);
                assert!(confirm);
                // default name preserved
                assert_eq!(script_name, "envelope-rules");
            }
            _ => panic!("expected rule publish-sieve command"),
        }
    }

    #[test]
    fn rule_publish_sieve_defaults_to_dry_run_safe_state() {
        // Without --dry-run or --confirm the parse must still succeed; the
        // handler is responsible for treating absent --confirm as
        // non-mutating. This test exercises clap's defaults so the runtime
        // safety check is not bypassed by clap accidentally requiring one
        // of the flags via group config.
        let cli = Cli::try_parse_from(["envelope", "rule", "publish-sieve", "--account", "acct-1"])
            .expect("publish-sieve should parse without an explicit mode flag");

        match cli.command {
            Commands::Rule {
                subcommand:
                    RuleCmd::PublishSieve {
                        dry_run, confirm, ..
                    },
            } => {
                assert!(!dry_run);
                assert!(!confirm);
            }
            _ => panic!("expected rule publish-sieve command"),
        }
    }

    #[test]
    fn inbox_accepts_limit_at_agent_max() {
        let cli = Cli::try_parse_from(["envelope", "inbox", "--limit", "1000"])
            .expect("inbox --limit 1000 should parse");
        match cli.command {
            Commands::Inbox { limit, .. } => assert_eq!(limit, 1000),
            _ => panic!("expected inbox command"),
        }
    }

    #[test]
    fn inbox_rejects_limit_above_agent_max() {
        let err = match Cli::try_parse_from(["envelope", "inbox", "--limit", "1001"]) {
            Ok(_) => panic!("inbox --limit 1001 must be rejected by clap before any IMAP work"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("--limit") && msg.contains("1000"),
            "expected --limit max-of-1000 error, got: {msg}"
        );
    }

    #[test]
    fn inbox_rejects_zero_limit() {
        let err = match Cli::try_parse_from(["envelope", "inbox", "--limit", "0"]) {
            Ok(_) => panic!("inbox --limit 0 must be rejected by clap"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("--limit"),
            "expected --limit validation error, got: {err}"
        );
    }

    #[test]
    fn search_accepts_limit_at_agent_max() {
        let cli = Cli::try_parse_from(["envelope", "search", "ALL", "--limit", "1000"])
            .expect("search --limit 1000 should parse");
        match cli.command {
            Commands::Search { limit, .. } => assert_eq!(limit, 1000),
            _ => panic!("expected search command"),
        }
    }

    #[test]
    fn search_rejects_limit_above_agent_max() {
        let err = match Cli::try_parse_from(["envelope", "search", "ALL", "--limit", "1001"]) {
            Ok(_) => panic!("search --limit 1001 must be rejected by clap before any IMAP work"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("--limit") && msg.contains("1000"),
            "expected --limit max-of-1000 error, got: {msg}"
        );
    }

    #[test]
    fn search_rejects_zero_limit() {
        let err = match Cli::try_parse_from(["envelope", "search", "ALL", "--limit", "0"]) {
            Ok(_) => panic!("search --limit 0 must be rejected by clap"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("--limit"),
            "expected --limit validation error, got: {err}"
        );
    }
}
