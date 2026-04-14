// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "envelope",
    version,
    about = "Email mastery for agents. BYO mailbox — give it an email and password, it does the rest.",
    after_help = r#"GETTING STARTED
  Add an account (auto-discovers IMAP/SMTP from the domain):
    envelope accounts add --email you@gmail.com --password <app-password>

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

AGENT USAGE
  Every command supports --json for machine consumption:
    envelope inbox --json | jq '.[0].subject'
    envelope folders --json | jq '.[] | {name: .folder, unseen}'
    envelope snooze list --json

PROVIDERS
  Envelope auto-discovers IMAP/SMTP servers via DNS. Tested with:
    Gmail (app password), Outlook.com, Microsoft Workmail,
    Migadu, Fastmail, self-hosted Dovecot, generic IMAP.

MORE HELP
  envelope <command> --help    Show help for a specific command
  envelope serve               Open the web dashboard at http://localhost:3141
  https://github.com/tymrtn/envelope-email"#
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
        /// Maximum messages to return
        #[arg(long, default_value = "25")]
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
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Maximum results
        #[arg(long, default_value = "25")]
        limit: u32,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
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
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
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

    /// Delete a message
    Delete {
        /// Message UID
        uid: u32,
        /// IMAP folder
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
    },

    /// Manage message flags
    Flag {
        #[command(subcommand)]
        subcommand: FlagCmd,
    },

    /// List IMAP folders
    Folders {
        /// Account ID or email
        #[arg(long)]
        account: Option<String>,
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

    /// Start the localhost dashboard
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "3141")]
        port: u16,
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
    },
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// Add a new email account
    Add {
        /// Email address
        #[arg(long)]
        email: String,
        /// Password (will prompt if not given)
        #[arg(long)]
        password: Option<String>,
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
    },
    /// List configured accounts
    List,
    /// Remove an account
    Remove {
        /// Account ID or email address
        id: String,
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
        /// CC recipient(s)
        #[arg(long)]
        cc: Option<String>,
        /// BCC recipient(s)
        #[arg(long)]
        bcc: Option<String>,
        /// In-Reply-To Message-ID (for replies)
        #[arg(long)]
        in_reply_to: Option<String>,
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
    /// Activate a license key
    Activate {
        /// License key
        key: String,
    },
    /// Show current license status
    Status,
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
        /// Action: move=Folder, flag=name, unflag=name, delete, unsubscribe, tag=name
        #[arg(long)]
        action: String,
        /// Priority (lower runs first)
        #[arg(long, default_value = "100")]
        priority: i64,
        /// Stop evaluating further rules after this one fires
        #[arg(long)]
        stop: bool,
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
    /// Batch-apply rules to messages in a folder
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
        } => commands::search::run(
            &query,
            &folder,
            limit,
            account.as_deref(),
            cli.json,
            backend,
        ),

        Commands::Send {
            to,
            subject,
            body,
            html,
            cc,
            bcc,
            reply_to,
            attach,
            account,
        } => commands::send::run(
            &to,
            &subject,
            body.as_deref(),
            html.as_deref(),
            cc.as_deref(),
            bcc.as_deref(),
            reply_to.as_deref(),
            &attach,
            account.as_deref(),
            cli.json,
            backend,
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
            account,
        } => commands::messages::run_delete(uid, &folder, account.as_deref(), cli.json, backend),

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
                cc,
                bcc,
                in_reply_to,
            } => commands::drafts::run_create(
                &to,
                subject.as_deref(),
                body.as_deref(),
                account.as_deref(),
                cli.json,
                backend,
                cc.as_deref(),
                bcc.as_deref(),
                in_reply_to.as_deref(),
            ),
            DraftCmd::Send { id, account } => {
                commands::drafts::run_send(&id, account.as_deref(), cli.json, backend)
            }
            DraftCmd::Discard { id, account } => {
                commands::drafts::run_discard(&id, cli.json, account.as_deref(), backend)
            }
        },

        Commands::Serve { port } => commands::serve::run(port),
        Commands::Compose { .. } => {
            eprintln!("License required — visit https://envelope-email.dev");
            std::process::exit(1);
        }
        Commands::License { subcommand } => match subcommand {
            LicenseCmd::Activate { key } => {
                eprintln!("Not yet implemented: license activate (key: {key})");
                std::process::exit(1);
            }
            LicenseCmd::Status => {
                println!("License: unlicensed (free tier)");
                Ok(())
            }
        },
        Commands::Attributes { .. } => {
            eprintln!("Not yet implemented: attributes");
            std::process::exit(1);
        }
        Commands::Actions { subcommand } => match subcommand {
            ActionsCmd::Tail { .. } => {
                eprintln!("Not yet implemented: actions tail");
                std::process::exit(1);
            }
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
            SnoozeCmd::Cancel { uid, account } => {
                commands::snooze::run_unsnooze(uid, account.as_deref(), cli.json, backend)
            }
        },

        Commands::Unsnooze { once: _, account } => {
            commands::snooze::run_check(account.as_deref(), cli.json, backend)
        }

        Commands::Thread { subcommand } => match subcommand {
            ThreadCmd::Show {
                uid,
                folder,
                account,
            } => commands::thread::run_show(uid, &folder, account.as_deref(), cli.json, backend),
            ThreadCmd::List {
                account,
                limit,
            } => commands::thread::run_list(account.as_deref(), limit, cli.json, backend),
            ThreadCmd::Build { account, limit } => {
                commands::thread::run_build(account.as_deref(), limit, cli.json, backend)
            }
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

        Commands::Rule { subcommand } => match subcommand {
            RuleCmd::Create {
                name,
                match_from,
                match_to,
                match_subject,
                match_tag,
                match_score_above,
                match_score_below,
                action,
                priority,
                stop,
                account,
            } => commands::rule::run_create(
                &name,
                match_from.as_deref(),
                match_to.as_deref(),
                match_subject.as_deref(),
                &match_tag,
                &match_score_above,
                &match_score_below,
                &action,
                priority,
                stop,
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
            RuleCmd::Run {
                folder,
                limit,
                account,
            } => commands::rule::run_apply(
                &folder,
                account.as_deref(),
                limit,
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
        },

        Commands::Unsubscribe {
            uid,
            folder,
            account,
            confirm,
        } => commands::unsubscribe_cmd::run(
            uid,
            &folder,
            account.as_deref(),
            confirm,
            cli.json,
            backend,
        ),
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
