// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

mod commands;
mod governor;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "envelope-email", about = "BYO mailbox email client")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Credential storage backend: "file" (default) or "keychain"
    #[arg(long, global = true, default_value = "file")]
    credential_store: String,

    /// Bypass governor safety checks (emergency use)
    #[arg(long, global = true)]
    no_governor: bool,
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

    /// Governor safety integration — status and testing
    Governor {
        #[command(subcommand)]
        subcommand: GovernorCmd,
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
enum GovernorCmd {
    /// Show governor integration status
    Status,
    /// Dry-run a send through governor scoring
    TestSend {
        /// Recipient address to test
        #[arg(long)]
        to: String,
        /// Subject line to test
        #[arg(long, default_value = "test message")]
        subject: String,
    },
    /// Dry-run a destructive action through governor scoring
    TestDelete {
        /// Description of the action to test
        #[arg(long, default_value = "delete message UID 1 from INBOX")]
        action: String,
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

    let no_governor = cli.no_governor;

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

        // --- GOVERNED: send ---
        Commands::Send {
            to,
            subject,
            body,
            html,
            cc,
            bcc,
            reply_to,
            account,
        } => {
            if governor::is_enabled(no_governor) {
                let attrs = governor::GovernorAttrs::email_send(&to, &subject);
                if let Err(e) = governor::check(&attrs) {
                    Err(e)
                } else {
                    commands::send::run(
                        &to,
                        &subject,
                        body.as_deref(),
                        html.as_deref(),
                        cc.as_deref(),
                        bcc.as_deref(),
                        reply_to.as_deref(),
                        account.as_deref(),
                        cli.json,
                        backend,
                    )
                }
            } else {
                commands::send::run(
                    &to,
                    &subject,
                    body.as_deref(),
                    html.as_deref(),
                    cc.as_deref(),
                    bcc.as_deref(),
                    reply_to.as_deref(),
                    account.as_deref(),
                    cli.json,
                    backend,
                )
            }
        }

        // --- GOVERNED: move (only if destination is destructive) ---
        Commands::Move {
            uid,
            to_folder,
            folder,
            account,
        } => {
            if governor::is_enabled(no_governor) && governor::is_destructive_folder(&to_folder) {
                let desc = format!("move UID {uid} from {folder} to {to_folder}");
                let attrs = governor::GovernorAttrs::destructive(&desc);
                if let Err(e) = governor::check(&attrs) {
                    Err(e)
                } else {
                    commands::messages::run_move(
                        uid,
                        &folder,
                        &to_folder,
                        account.as_deref(),
                        cli.json,
                        backend,
                    )
                }
            } else {
                commands::messages::run_move(
                    uid,
                    &folder,
                    &to_folder,
                    account.as_deref(),
                    cli.json,
                    backend,
                )
            }
        }

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

        // --- GOVERNED: delete ---
        Commands::Delete {
            uid,
            folder,
            account,
        } => {
            if governor::is_enabled(no_governor) {
                let desc = format!("delete UID {uid} from {folder}");
                let attrs = governor::GovernorAttrs::destructive(&desc);
                if let Err(e) = governor::check(&attrs) {
                    Err(e)
                } else {
                    commands::messages::run_delete(
                        uid,
                        &folder,
                        account.as_deref(),
                        cli.json,
                        backend,
                    )
                }
            } else {
                commands::messages::run_delete(uid, &folder, account.as_deref(), cli.json, backend)
            }
        }

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

        // --- GOVERNED: draft send (outbound email) ---
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
                if governor::is_enabled(no_governor) {
                    // We need draft info for governor attrs — resolve it here
                    let attrs =
                        governor::GovernorAttrs::email_send(&format!("draft:{id}"), "draft send");
                    if let Err(e) = governor::check(&attrs) {
                        Err(e)
                    } else {
                        commands::drafts::run_send(&id, account.as_deref(), cli.json, backend)
                    }
                } else {
                    commands::drafts::run_send(&id, account.as_deref(), cli.json, backend)
                }
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

        // --- Governor subcommand ---
        Commands::Governor { subcommand } => match subcommand {
            GovernorCmd::Status => commands::governor::run_status(cli.json),
            GovernorCmd::TestSend { to, subject } => {
                commands::governor::run_test_send(&to, &subject, cli.json)
            }
            GovernorCmd::TestDelete { action } => {
                commands::governor::run_test_delete(&action, cli.json)
            }
        },
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
