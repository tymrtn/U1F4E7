// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! What a CLI command may do when `ENVELOPE_AGENT_TOKEN` is set.
//!
//! Every command is classified by [`token_mode_permission`]: read-only,
//! gated on named policy actions, or operator-only. The match is exhaustive
//! over every command and subcommand enum, with no catch-all arm, so a new
//! command does not compile until it is classified here. [`enforce`] runs
//! before dispatch.

use crate::commands::agent_context::{
    self, AgentContext, CliDenial, RULES_WRITE, SIEVE_PUBLISH, UNSUBSCRIBE, WATCH_WEBHOOK,
};
use crate::{
    AccountsCmd, ActionsCmd, ActionsExecCmd, AgentCmd, AgentPolicyCmd, AnalyticsCmd, AttachmentCmd,
    BackupCmd, BulkCmd, Commands, ConfigCmd, ContactsCmd, DeliverabilityCmd, DraftCmd,
    EventDeliveriesCmd, EventRoutesCmd, EventsCmd, EvidenceAttachmentCmd, EvidenceCmd, FlagCmd,
    GovernorCmd, LicenseCmd, MigrateCmd, RuleCmd, ScheduledCmd, SignatureCmd, SnoozeCmd, TagCmd,
    ThreadCmd, ThreatCmd,
};
use envelope_email_transport::PolicyDenial;
use envelope_email_transport::threat::is_threat_tag;

/// How a command runs under an agent token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    /// Reads mail or local state; runs as it does for the operator.
    ReadOnly,
    /// Runs only when the agent's policy grants every listed action. The
    /// command may then apply more of the policy itself (account, ceiling,
    /// allowlist, human approval).
    Gated(&'static [&'static str]),
    /// Changes credentials, identities, policy, configuration,
    /// authentication, delivery routes, a threat verdict, or another
    /// person's decision. Never runs with an agent token.
    OperatorOnly,
}

use Permission::{Gated, OperatorOnly, ReadOnly};

const SEND: &[&str] = &["send"];
const DRAFT_CREATE: &[&str] = &["draft.create"];
const DRAFT_MODIFY: &[&str] = &["draft.modify"];
const MOVE: &[&str] = &["move"];
const DELETE: &[&str] = &["delete"];
const FLAG: &[&str] = &["flag"];
const TAG: &[&str] = &["tag"];
const SNOOZE: &[&str] = &["snooze"];
const BULK_MOVE: &[&str] = &["bulk", "move"];
const BULK_FLAG: &[&str] = &["bulk", "flag"];
const BULK_DELETE: &[&str] = &["bulk", "delete"];
const BULK_TAG: &[&str] = &["bulk", "tag"];
const RULES_RUN: &[&str] = &["rules.run"];
const RULES_EDIT: &[&str] = &[RULES_WRITE];
const SIEVE: &[&str] = &[SIEVE_PUBLISH];
const WATCH_HOOK: &[&str] = &[WATCH_WEBHOOK];
const WATCH_HOOK_AND_RULES: &[&str] = &[WATCH_WEBHOOK, "rules.run"];
const LIST_UNSUBSCRIBE: &[&str] = &[UNSUBSCRIBE];

/// The permission class of `command` under an agent token.
pub(crate) fn token_mode_permission(command: &Commands) -> Permission {
    match command {
        Commands::Accounts { subcommand } => match subcommand {
            AccountsCmd::List => ReadOnly,
            AccountsCmd::SetupInstructions { copy_password, .. } => {
                if *copy_password {
                    OperatorOnly
                } else {
                    ReadOnly
                }
            }
            AccountsCmd::Signature { subcommand } => match subcommand {
                SignatureCmd::Show { .. } => ReadOnly,
                SignatureCmd::Set { .. } | SignatureCmd::Clear { .. } => OperatorOnly,
            },
            AccountsCmd::Add { .. }
            | AccountsCmd::Rekey
            | AccountsCmd::ImportKeychain { .. }
            | AccountsCmd::CopyPassword { .. }
            | AccountsCmd::Remove { .. } => OperatorOnly,
        },
        Commands::Inbox { .. }
        | Commands::Read { .. }
        | Commands::Search { .. }
        | Commands::Folders { .. }
        | Commands::Code { .. }
        | Commands::Paths
        | Commands::Quickstart { .. }
        | Commands::Contract { .. }
        | Commands::Compose { .. }
        | Commands::Attributes { .. }
        // MCP authorizes every tool call against the agent's policy itself.
        | Commands::Mcp { .. } => ReadOnly,
        // --repair copies the database and credentials to --backup-dir.
        Commands::Doctor { repair, .. } => {
            if *repair {
                OperatorOnly
            } else {
                ReadOnly
            }
        }
        Commands::Send { .. } => Gated(SEND),
        Commands::Move { .. } | Commands::Copy { .. } => Gated(MOVE),
        Commands::Delete { .. } => Gated(DELETE),
        Commands::Flag { subcommand } => match subcommand {
            FlagCmd::Add { .. } | FlagCmd::Remove { .. } => Gated(FLAG),
        },
        Commands::Bulk { subcommand } => match subcommand {
            BulkCmd::Move { .. } | BulkCmd::Copy { .. } => Gated(BULK_MOVE),
            BulkCmd::Flag { .. } => Gated(BULK_FLAG),
            BulkCmd::Delete { .. } => Gated(BULK_DELETE),
            BulkCmd::Tag { tag, .. } => {
                if is_threat_tag(tag) {
                    OperatorOnly
                } else {
                    Gated(BULK_TAG)
                }
            }
        },
        Commands::Migrate { subcommand } => match subcommand {
            MigrateCmd::Folders { .. } | MigrateCmd::Run { .. } => OperatorOnly,
        },
        Commands::Backup { subcommand } => match subcommand {
            BackupCmd::Export { .. } | BackupCmd::Verify { .. } | BackupCmd::AuditState { .. } => {
                ReadOnly
            }
            BackupCmd::Restore { .. } => OperatorOnly,
        },
        Commands::Evidence { subcommand } => match subcommand {
            EvidenceCmd::Collect { .. } | EvidenceCmd::Verify { .. } => ReadOnly,
            // --unsafe overrides the threat engine's attachment block.
            EvidenceCmd::Attachment(EvidenceAttachmentCmd::Export { allow_unsafe, .. }) => {
                if *allow_unsafe {
                    OperatorOnly
                } else {
                    ReadOnly
                }
            }
        },
        Commands::Deliverability { subcommand } => match subcommand {
            DeliverabilityCmd::Check { .. } => ReadOnly,
        },
        Commands::Attachment { subcommand } => match subcommand {
            AttachmentCmd::List { .. } => ReadOnly,
            // --unsafe overrides the threat engine's attachment block.
            AttachmentCmd::Download { allow_unsafe, .. } => {
                if *allow_unsafe {
                    OperatorOnly
                } else {
                    ReadOnly
                }
            }
        },
        Commands::Draft { subcommand } => match subcommand {
            DraftCmd::Show { .. } | DraftCmd::List { .. } => ReadOnly,
            DraftCmd::Create { .. } | DraftCmd::Reply { .. } | DraftCmd::Forward { .. } => {
                Gated(DRAFT_CREATE)
            }
            DraftCmd::Edit { .. } | DraftCmd::Discard { .. } => Gated(DRAFT_MODIFY),
            DraftCmd::Send { .. } => Gated(SEND),
        },
        Commands::Governor { subcommand } => match subcommand {
            GovernorCmd::Catalog => ReadOnly,
        },
        Commands::Serve { .. } => OperatorOnly,
        Commands::License { subcommand } => match subcommand {
            LicenseCmd::Activate { .. } | LicenseCmd::Status | LicenseCmd::Deactivate => {
                OperatorOnly
            }
        },
        Commands::Agent { subcommand } => match subcommand {
            AgentCmd::List | AgentCmd::Show { .. } => ReadOnly,
            AgentCmd::Create { .. } | AgentCmd::Revoke { .. } => OperatorOnly,
            AgentCmd::Policy { subcommand } => match subcommand {
                AgentPolicyCmd::Show { .. } => ReadOnly,
                AgentPolicyCmd::Set { .. } => OperatorOnly,
            },
        },
        Commands::Actions { subcommand } => match subcommand {
            ActionsCmd::Tail { .. } => ReadOnly,
            // Records a local audit note; changes no mail. An agent records
            // it as itself (see `check_actor`).
            ActionsCmd::Exec { subcommand, .. } => match subcommand {
                ActionsExecCmd::MarkHandled => ReadOnly,
            },
            // A person's answer to an offered action.
            ActionsCmd::Confirm { .. } | ActionsCmd::Dismiss { .. } => OperatorOnly,
        },
        Commands::Threat { subcommand } => match subcommand {
            // Scanning applies the operator's own threat.quarantine setting.
            ThreatCmd::Scan { .. }
            | ThreatCmd::Show { .. }
            | ThreatCmd::Explain { .. }
            | ThreatCmd::Stats { .. } => ReadOnly,
            ThreatCmd::Report { .. } => Gated(DRAFT_CREATE),
            ThreatCmd::MarkSafe { .. } | ThreatCmd::Release { .. } => OperatorOnly,
        },
        Commands::Analytics { subcommand } => match subcommand {
            AnalyticsCmd::Show { .. } => ReadOnly,
        },
        Commands::Events { subcommand } => match subcommand {
            EventsCmd::List { .. } | EventsCmd::Ack { .. } => ReadOnly,
            EventsCmd::Routes { subcommand } => match subcommand {
                EventRoutesCmd::List { .. } => ReadOnly,
                EventRoutesCmd::Add { .. } | EventRoutesCmd::Remove { .. } => OperatorOnly,
            },
            EventsCmd::Deliveries { subcommand } => match subcommand {
                EventDeliveriesCmd::List { .. } => ReadOnly,
                EventDeliveriesCmd::Retry { .. } => OperatorOnly,
            },
        },
        Commands::Snooze { subcommand } => match subcommand {
            SnoozeCmd::List { .. } => ReadOnly,
            SnoozeCmd::Set { .. } | SnoozeCmd::CheckReplies { .. } | SnoozeCmd::Cancel { .. } => {
                Gated(SNOOZE)
            }
        },
        Commands::Unsnooze { .. } => Gated(SNOOZE),
        Commands::Scheduled { subcommand } => match subcommand {
            ScheduledCmd::List { .. } => ReadOnly,
            ScheduledCmd::Hold { .. } | ScheduledCmd::Cancel { .. } => Gated(DRAFT_MODIFY),
        },
        Commands::Thread { subcommand } => match subcommand {
            // Build writes only the local thread index.
            ThreadCmd::Show { .. } | ThreadCmd::List { .. } | ThreadCmd::Build { .. } => ReadOnly,
        },
        Commands::Tag { subcommand } => match subcommand {
            TagCmd::Show { .. } | TagCmd::List { .. } => ReadOnly,
            TagCmd::Set { tag, .. } => {
                if tag.iter().any(|t| is_threat_tag(t)) {
                    OperatorOnly
                } else {
                    Gated(TAG)
                }
            }
        },
        Commands::Rule { subcommand } => match subcommand {
            RuleCmd::List { .. }
            | RuleCmd::Test { .. }
            | RuleCmd::Preview { .. }
            | RuleCmd::Export { .. } => ReadOnly,
            // `rule create` and `rule enable` refuse an agent a rule that sets
            // a threat:* tag once the action is parsed.
            RuleCmd::Create { .. }
            | RuleCmd::Enable { .. }
            | RuleCmd::Disable { .. }
            | RuleCmd::Delete { .. } => Gated(RULES_EDIT),
            // Without --confirm, `rule run` only explains itself.
            RuleCmd::Run { confirm, .. } => {
                if *confirm {
                    Gated(RULES_RUN)
                } else {
                    ReadOnly
                }
            }
            // --host sends the mailbox password to the named server. Without
            // --confirm, publish-sieve is a dry run.
            RuleCmd::PublishSieve { host, confirm, .. } => {
                if host.is_some() {
                    OperatorOnly
                } else if *confirm {
                    Gated(SIEVE)
                } else {
                    ReadOnly
                }
            }
        },
        Commands::Contacts { subcommand } => match subcommand {
            ContactsCmd::List { .. } | ContactsCmd::Show { .. } => ReadOnly,
            // These record a person as the curator, which vouches for the address.
            ContactsCmd::Add { .. }
            | ContactsCmd::Import { .. }
            | ContactsCmd::Tag { .. }
            | ContactsCmd::Untag { .. } => OperatorOnly,
        },
        // Without --confirm, unsubscribe is a dry run.
        Commands::Unsubscribe { confirm, .. } => {
            if *confirm {
                Gated(LIST_UNSUBSCRIBE)
            } else {
                ReadOnly
            }
        }
        // --webhook and --deliver push each new message to a URL.
        Commands::Watch {
            webhook,
            deliver,
            run_rules,
            ..
        } => match (webhook.is_some() || *deliver, *run_rules) {
            (false, false) => ReadOnly,
            (true, false) => Gated(WATCH_HOOK),
            (false, true) => Gated(RULES_RUN),
            (true, true) => Gated(WATCH_HOOK_AND_RULES),
        },
        Commands::Config { subcommand } => match subcommand {
            ConfigCmd::Get { .. } => ReadOnly,
            ConfigCmd::Set { .. } | ConfigCmd::Unset { .. } => OperatorOnly,
        },
    }
}

/// Check `command` against the agent token, if one is set, before it runs.
///
/// No (or a blank) token: the operator; nothing changes. Any other token must
/// belong to an active agent, or every command fails closed with
/// `agent_token_invalid`. Then the command's [`Permission`] decides.
pub(crate) fn enforce(command: &Commands, json: bool) -> anyhow::Result<()> {
    // `envelope mcp` resolves the token at startup and refuses to start on a
    // bad one, then authorizes every tool call itself.
    if matches!(command, Commands::Mcp { .. }) {
        return Ok(());
    }
    let agent = match agent_context::peek_cli_agent() {
        Ok(Some(agent)) => agent,
        Ok(None) => return Ok(()),
        Err(e) => return Err(agent_context::print_cli_denial(e, json)),
    };
    decide(token_mode_permission(command), &agent)
        .and_then(|()| check_actor(command, &agent))
        .map_err(|denial| {
            record_denial(&agent, &denial);
            agent_context::print_cli_denial(CliDenial(denial).into(), json)
        })
}

/// The pure decision for one command and one agent.
fn decide(permission: Permission, agent: &AgentContext) -> Result<(), PolicyDenial> {
    match permission {
        ReadOnly => Ok(()),
        Gated(actions) => actions
            .iter()
            .try_for_each(|action| agent.allows_action(action)),
        OperatorOnly => Err(agent_context::operator_only_denial()),
    }
}

/// `actions exec --actor` under a token: an agent records an action under
/// its own name or id, never another's.
fn check_actor(command: &Commands, agent: &AgentContext) -> Result<(), PolicyDenial> {
    if let Commands::Actions {
        subcommand: ActionsCmd::Exec { actor, .. },
    } = command
        && actor != &agent.agent_name
        && actor != &agent.agent_id
    {
        return Err(PolicyDenial {
            code: agent_context::OPERATOR_ONLY_CODE,
            reason: format!(
                "only the operator records an action under another actor; pass --actor {}",
                agent.agent_name
            ),
        });
    }
    Ok(())
}

/// File the refusal in the agent's action log. The account is not resolved
/// before dispatch, so the row goes under `(unresolved)`, as MCP does.
fn record_denial(agent: &AgentContext, denial: &PolicyDenial) {
    let logged = envelope_email_store::Database::open_default().and_then(|db| {
        db.log_denied_action_with_agent(
            "(unresolved)",
            "cli_command",
            denial.code,
            Some(&agent.agent_id),
        )
    });
    if let Err(e) = logged {
        tracing::warn!("could not record the refused command in the action log: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::{CommandFactory, Parser};
    use envelope_email_transport::{AgentPolicy, SendMode};
    use std::collections::BTreeSet;

    /// One argv per command path, flag-dependent paths more than once, with
    /// its class. Positional values are placeholders; nothing runs.
    const MATRIX: &[(&str, Permission)] = &[
        ("accounts add --email a@b.test", OperatorOnly),
        ("accounts rekey", OperatorOnly),
        ("accounts import-keychain --email a@b.test", OperatorOnly),
        ("accounts list", ReadOnly),
        ("accounts setup-instructions --account a", ReadOnly),
        (
            "accounts setup-instructions --account a --copy-password",
            OperatorOnly,
        ),
        ("accounts copy-password --account a", OperatorOnly),
        ("accounts remove a", OperatorOnly),
        ("accounts signature show --account a", ReadOnly),
        ("accounts signature set --account a --text t", OperatorOnly),
        ("accounts signature clear --account a", OperatorOnly),
        ("inbox", ReadOnly),
        ("read 1", ReadOnly),
        ("search ALL", ReadOnly),
        ("send --to a@b.test --subject s", Gated(SEND)),
        ("move 1 --to-folder X", Gated(MOVE)),
        ("copy 1 --to-folder X", Gated(MOVE)),
        ("delete 1", Gated(DELETE)),
        ("flag add 1 seen", Gated(FLAG)),
        ("flag remove 1 seen", Gated(FLAG)),
        ("bulk move --to-folder X --uids 1", Gated(BULK_MOVE)),
        ("bulk copy --to-folder X --uids 1", Gated(BULK_MOVE)),
        (
            "bulk flag --flag seen --action add --uids 1",
            Gated(BULK_FLAG),
        ),
        ("bulk delete --uids 1", Gated(BULK_DELETE)),
        ("bulk tag --tag t --uids 1", Gated(BULK_TAG)),
        ("bulk tag --tag threat:malware --uids 1", OperatorOnly),
        ("folders", ReadOnly),
        ("migrate folders --from a --to b", OperatorOnly),
        ("migrate run --from a --to b", OperatorOnly),
        ("backup export --account a", ReadOnly),
        ("backup verify --from /x", ReadOnly),
        ("backup restore --account a --from /x", OperatorOnly),
        ("backup audit-state --account a --from /x", ReadOnly),
        (
            "evidence collect --account a --folder INBOX --out /x --query ALL",
            ReadOnly,
        ),
        ("evidence verify --from /x", ReadOnly),
        (
            "evidence attachment export --account a --uid 1 --out /x",
            ReadOnly,
        ),
        (
            "evidence attachment export --account a --uid 1 --out /x --unsafe",
            OperatorOnly,
        ),
        ("deliverability check --domain example.test", ReadOnly),
        ("attachment list 1", ReadOnly),
        ("attachment download 1 f", ReadOnly),
        ("attachment download 1 f --unsafe", OperatorOnly),
        ("draft create --to a@b.test", Gated(DRAFT_CREATE)),
        ("draft reply 1", Gated(DRAFT_CREATE)),
        ("draft forward 1", Gated(DRAFT_CREATE)),
        ("draft edit d", Gated(DRAFT_MODIFY)),
        ("draft show d", ReadOnly),
        ("draft list", ReadOnly),
        ("draft send d", Gated(SEND)),
        ("draft discard d", Gated(DRAFT_MODIFY)),
        ("governor catalog", ReadOnly),
        ("serve", OperatorOnly),
        ("compose", ReadOnly),
        ("license activate --key-stdin", OperatorOnly),
        ("license status", OperatorOnly),
        ("license deactivate", OperatorOnly),
        ("agent create n", OperatorOnly),
        ("agent list", ReadOnly),
        ("agent show n", ReadOnly),
        ("agent revoke n", OperatorOnly),
        ("agent policy set n", OperatorOnly),
        ("agent policy show n", ReadOnly),
        ("attributes", ReadOnly),
        ("actions tail", ReadOnly),
        ("actions confirm e", OperatorOnly),
        ("actions dismiss e", OperatorOnly),
        ("actions exec --event-id e --actor a mark-handled", ReadOnly),
        ("threat scan", ReadOnly),
        ("threat show 1", ReadOnly),
        ("threat explain 1", ReadOnly),
        ("threat mark-safe 1", OperatorOnly),
        ("threat release 1", OperatorOnly),
        ("threat report 1", Gated(DRAFT_CREATE)),
        ("threat stats", ReadOnly),
        ("analytics show 1", ReadOnly),
        ("events list", ReadOnly),
        ("events ack e", ReadOnly),
        ("events routes add --url https://h.example/x", OperatorOnly),
        ("events routes list", ReadOnly),
        ("events routes remove r", OperatorOnly),
        ("events deliveries list", ReadOnly),
        ("events deliveries retry d", OperatorOnly),
        ("snooze set 1 --until 2h", Gated(SNOOZE)),
        ("snooze list", ReadOnly),
        ("snooze check-replies", Gated(SNOOZE)),
        ("snooze cancel 1", Gated(SNOOZE)),
        ("unsnooze --once", Gated(SNOOZE)),
        ("scheduled list", ReadOnly),
        ("scheduled hold d", Gated(DRAFT_MODIFY)),
        ("scheduled cancel d", Gated(DRAFT_MODIFY)),
        ("thread show 1", ReadOnly),
        ("thread list", ReadOnly),
        ("thread build", ReadOnly),
        ("tag set 1 --tag t", Gated(TAG)),
        (
            "tag set 1 --tag ok --tag Threat:False_Positive",
            OperatorOnly,
        ),
        ("tag show 1", ReadOnly),
        ("tag list", ReadOnly),
        (
            "rule create --name n --match-from * --action delete",
            Gated(RULES_EDIT),
        ),
        ("rule list", ReadOnly),
        ("rule test 1", ReadOnly),
        ("rule preview", ReadOnly),
        ("rule run", ReadOnly),
        ("rule run --confirm", Gated(RULES_RUN)),
        ("rule enable n", Gated(RULES_EDIT)),
        ("rule disable n", Gated(RULES_EDIT)),
        ("rule delete n", Gated(RULES_EDIT)),
        ("rule export", ReadOnly),
        ("rule publish-sieve", ReadOnly),
        ("rule publish-sieve --confirm", Gated(SIEVE)),
        ("rule publish-sieve --host h.example", OperatorOnly),
        (
            "rule publish-sieve --confirm --host h.example",
            OperatorOnly,
        ),
        ("contacts add --email a@b.test", OperatorOnly),
        ("contacts list", ReadOnly),
        ("contacts show a@b.test", ReadOnly),
        ("contacts tag a@b.test --tag vip", OperatorOnly),
        ("contacts untag a@b.test --tag vip", OperatorOnly),
        ("contacts import", OperatorOnly),
        ("unsubscribe 1", ReadOnly),
        ("unsubscribe 1 --confirm", Gated(LIST_UNSUBSCRIBE)),
        ("code --from a@b.test", ReadOnly),
        ("watch", ReadOnly),
        ("watch --deliver", Gated(WATCH_HOOK)),
        ("watch --deliver --run-rules", Gated(WATCH_HOOK_AND_RULES)),
        ("watch --webhook https://h.example/x", Gated(WATCH_HOOK)),
        ("watch --run-rules", Gated(RULES_RUN)),
        (
            "watch --webhook https://h.example/x --run-rules",
            Gated(WATCH_HOOK_AND_RULES),
        ),
        ("paths", ReadOnly),
        ("doctor", ReadOnly),
        ("doctor --repair", OperatorOnly),
        ("config get k", ReadOnly),
        ("config set k v", OperatorOnly),
        ("config unset k", OperatorOnly),
        ("quickstart", ReadOnly),
        ("contract", ReadOnly),
        ("mcp", ReadOnly),
    ];

    fn parse(argv: &str) -> Cli {
        let args = std::iter::once("envelope").chain(argv.split_whitespace());
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("`{argv}` does not parse: {e}"))
    }

    /// Every leaf command path the CLI exposes, e.g. `rule publish-sieve`.
    fn leaf_paths(cmd: &clap::Command, prefix: &str, out: &mut BTreeSet<String>) {
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            let path = format!("{prefix}{}", sub.get_name());
            if sub.has_subcommands() {
                leaf_paths(sub, &format!("{path} "), out);
            } else {
                out.insert(path);
            }
        }
    }

    /// The command path an argv names, e.g. `actions exec mark-handled`.
    fn path_of(argv: &str) -> String {
        let args = std::iter::once("envelope").chain(argv.split_whitespace());
        let mut matches = Cli::command()
            .try_get_matches_from(args)
            .unwrap_or_else(|e| panic!("`{argv}` does not parse: {e}"));
        let mut path = Vec::new();
        while let Some((name, sub)) = matches.remove_subcommand() {
            path.push(name);
            matches = sub;
        }
        path.join(" ")
    }

    fn agent(actions: &[&str], ceiling: SendMode) -> AgentContext {
        AgentContext {
            agent_id: "agent-1".to_string(),
            agent_name: "skippy".to_string(),
            policy: AgentPolicy {
                allowed_accounts: vec!["*".to_string()],
                allowed_folders: vec!["*".to_string()],
                allowed_actions: actions.iter().map(|a| a.to_string()).collect(),
                send_mode_ceiling: ceiling,
                allow_recipients: Vec::new(),
            },
        }
    }

    #[test]
    fn every_command_is_classified_for_token_mode() {
        let mut leaves = BTreeSet::new();
        leaf_paths(&Cli::command(), "", &mut leaves);

        let mut covered = BTreeSet::new();
        for (argv, expected) in MATRIX {
            let cli = parse(argv);
            assert_eq!(token_mode_permission(&cli.command), *expected, "`{argv}`");
            covered.insert(path_of(argv));
        }
        let missing: Vec<_> = leaves.difference(&covered).collect();
        assert!(
            missing.is_empty(),
            "classify these commands in MATRIX: {missing:?}"
        );
    }

    #[test]
    fn a_draft_only_token_runs_reads_is_refused_operator_commands_and_named_actions() {
        // The default policy: every action by "*", draft-only ceiling.
        let default_agent = agent(&["*"], SendMode::DraftOnly);
        for (argv, permission) in MATRIX {
            let outcome = decide(*permission, &default_agent);
            match permission {
                ReadOnly => assert!(outcome.is_ok(), "`{argv}`"),
                OperatorOnly => {
                    assert_eq!(
                        outcome.unwrap_err().code,
                        agent_context::OPERATOR_ONLY_CODE,
                        "`{argv}`"
                    )
                }
                Gated(actions) => {
                    let named = actions
                        .iter()
                        .any(|a| agent_context::EXPLICIT_GRANT_ACTIONS.contains(a));
                    match outcome {
                        Ok(()) => assert!(!named, "`{argv}` needs a named grant"),
                        Err(denial) => {
                            assert!(named, "`{argv}`: {}", denial.reason);
                            assert_eq!(denial.code, "agent_policy_denied_action");
                        }
                    }
                }
            }
        }

        // A read-only policy is refused every gated command.
        let reader = agent(&["inbox.read"], SendMode::DraftOnly);
        for (argv, permission) in MATRIX {
            if let Gated(_) = permission {
                assert!(decide(*permission, &reader).is_err(), "`{argv}`");
            }
        }
    }

    /// The contract's name for an argv: its command path plus any flag that
    /// changes its class.
    fn contract_key(argv: &str) -> String {
        const CLASS_FLAGS: &[&str] = &[
            "--copy-password",
            "--unsafe",
            "--confirm",
            "--host",
            "--repair",
            "--webhook",
            "--deliver",
            "--run-rules",
        ];
        let mut key = path_of(argv);
        for word in argv.split_whitespace().filter(|w| CLASS_FLAGS.contains(w)) {
            key.push(' ');
            key.push_str(word);
        }
        if argv.to_lowercase().contains("threat:") {
            key.push_str(" with a threat:* tag");
        }
        key
    }

    #[test]
    fn the_contract_publishes_this_table() {
        let contract = crate::commands::contract::agent_contract();
        let gates = &contract["agent_identity"]["cli_token_gates"];
        let operator_only: BTreeSet<String> = gates["operator_only_commands"]
            .as_array()
            .expect("cli_token_gates.operator_only_commands")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let gated = gates["gated_commands"]
            .as_object()
            .expect("cli_token_gates.gated_commands");

        let mut seen = BTreeSet::new();
        for (argv, permission) in MATRIX {
            let key = contract_key(argv);
            match permission {
                ReadOnly => assert!(
                    !operator_only.contains(&key) && !gated.contains_key(&key),
                    "`{key}` is read-only"
                ),
                OperatorOnly => assert!(
                    operator_only.contains(&key),
                    "list `{key}` in operator_only_commands"
                ),
                Gated(actions) => assert_eq!(
                    gated.get(&key),
                    Some(&serde_json::json!(actions)),
                    "gated_commands[`{key}`]"
                ),
            }
            seen.insert(key);
        }
        let stale: Vec<_> = operator_only
            .iter()
            .chain(gated.keys())
            .filter(|key| !seen.contains(*key))
            .collect();
        assert!(stale.is_empty(), "not commands in MATRIX: {stale:?}");
    }

    #[test]
    fn an_agent_records_actions_only_as_itself() {
        let skippy = agent(&["*"], SendMode::DraftOnly);
        let exec = |actor: &str| {
            parse(&format!(
                "actions exec --event-id e --actor {actor} mark-handled"
            ))
            .command
        };
        assert!(check_actor(&exec("skippy"), &skippy).is_ok());
        assert!(check_actor(&exec("agent-1"), &skippy).is_ok());
        for other in ["tyler", "Skippy", "agent-2"] {
            let denial = check_actor(&exec(other), &skippy).unwrap_err();
            assert_eq!(denial.code, agent_context::OPERATOR_ONLY_CODE, "{other}");
        }
        assert!(check_actor(&parse("actions tail").command, &skippy).is_ok());
    }

    #[test]
    fn gated_commands_need_every_listed_action() {
        let only_bulk = agent(&["bulk"], SendMode::DraftOnly);
        assert!(decide(Gated(BULK_DELETE), &only_bulk).is_err());
        let both = agent(&["bulk", "delete"], SendMode::DraftOnly);
        assert!(decide(Gated(BULK_DELETE), &both).is_ok());
        let hook_only = agent(&[WATCH_WEBHOOK], SendMode::DraftOnly);
        assert!(decide(Gated(WATCH_HOOK_AND_RULES), &hook_only).is_err());
    }
}
