// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;

use super::common::setup_credentials;
use super::ui;

#[tokio::main]
pub async fn run_move(
    uid: u32,
    folder: &str,
    to_folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::move_message(&mut client, uid, folder, to_folder).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "move",
                "uid": uid,
                "from": folder,
                "to": to_folder,
                "ui": ui::message_ui(&creds.account.id, uid, to_folder),
            })
        );
    } else {
        println!("Moved UID {uid} from {folder} to {to_folder}");
    }

    Ok(())
}

#[tokio::main]
pub async fn run_copy(
    uid: u32,
    folder: &str,
    to_folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::copy_message(&mut client, uid, folder, to_folder).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "copy",
                "uid": uid,
                "from": folder,
                "to": to_folder,
                "ui": ui::message_ui(&creds.account.id, uid, to_folder),
            })
        );
    } else {
        println!("Copied UID {uid} from {folder} to {to_folder}");
    }

    Ok(())
}

/// What `envelope delete` will do, decided before any IMAP call so the choice
/// is unit-testable and the JSON output can say exactly what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletePlan {
    /// Default: reversible — move the message to the account's Trash.
    MoveToTrash,
    /// `--permanent` without `--confirm`: report what would be expunged, touch nothing.
    DryRunPermanent,
    /// `--permanent --confirm`: `\\Deleted` + EXPUNGE. Irreversible.
    Expunge,
    /// Plain delete inside Trash: Trash → Trash is a no-op, and the only
    /// meaningful delete there is permanent, which must be asked for explicitly.
    RefuseInTrash,
}

pub fn delete_plan(in_trash: bool, permanent: bool, confirm: bool) -> DeletePlan {
    match (permanent, confirm, in_trash) {
        (true, true, _) => DeletePlan::Expunge,
        (true, false, _) => DeletePlan::DryRunPermanent,
        (false, _, true) => DeletePlan::RefuseInTrash,
        (false, _, false) => DeletePlan::MoveToTrash,
    }
}

/// Provider-agnostic "is this the Trash mailbox?" check used when the account's
/// special-use detection is unavailable: compares the leaf name.
fn looks_like_trash(folder: &str) -> bool {
    let leaf = folder
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(folder)
        .trim()
        .to_ascii_lowercase();
    matches!(
        leaf.as_str(),
        "trash" | "deleted items" | "deleted messages"
    )
}

#[tokio::main]
pub async fn run_delete(
    uid: u32,
    folder: &str,
    permanent: bool,
    confirm: bool,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    // Resolve the account's real Trash once: it is both the default destination
    // and the way we know whether `folder` already IS Trash.
    let trash = envelope_email_transport::folders::resolve_move_destination(
        &mut client,
        &db,
        &creds.account.id,
        "\\Trash",
    )
    .await
    .context("failed to resolve the Trash folder")?;
    let in_trash = trash
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case(folder))
        .unwrap_or(false)
        || looks_like_trash(folder);

    match delete_plan(in_trash, permanent, confirm) {
        DeletePlan::MoveToTrash => {
            let Some(trash) = trash else {
                anyhow::bail!(
                    "this account has no Trash folder to move into; use --permanent --confirm to delete UID {uid} forever"
                );
            };
            envelope_email_transport::imap::move_message(&mut client, uid, folder, &trash).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "action": "delete",
                        "mode": "trashed",
                        "uid": uid,
                        "folder": folder,
                        "to": trash,
                        "reversible": true,
                        "ui": ui::message_ui(&creds.account.id, uid, &trash),
                    })
                );
            } else {
                println!(
                    "Moved UID {uid} from {folder} to {trash} (reversible; --permanent --confirm deletes forever)"
                );
            }
        }
        DeletePlan::DryRunPermanent => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "action": "delete",
                        "mode": "dry_run",
                        "would": "expunge",
                        "uid": uid,
                        "folder": folder,
                        "note": "pass --confirm with --permanent to delete forever",
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            } else {
                println!(
                    "Dry run: would permanently delete UID {uid} from {folder}. Re-run with --confirm to expunge."
                );
            }
        }
        DeletePlan::RefuseInTrash => {
            anyhow::bail!(
                "UID {uid} is already in {folder}; use --permanent --confirm to delete it forever"
            );
        }
        DeletePlan::Expunge => {
            envelope_email_transport::imap::delete_message(&mut client, folder, uid).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "action": "delete",
                        "mode": "expunged",
                        "uid": uid,
                        "folder": folder,
                        "reversible": false,
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            } else {
                println!("Permanently deleted UID {uid} from {folder}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod delete_plan_tests {
    use super::*;

    #[test]
    fn default_delete_outside_trash_moves_to_trash() {
        assert_eq!(
            delete_plan(false, false, false),
            DeletePlan::MoveToTrash,
            "Gmail semantics: delete is reversible by default"
        );
    }

    #[test]
    fn permanent_without_confirm_is_a_dry_run_never_an_expunge() {
        assert_eq!(delete_plan(false, true, false), DeletePlan::DryRunPermanent);
        assert_eq!(delete_plan(true, true, false), DeletePlan::DryRunPermanent);
    }

    #[test]
    fn permanent_with_confirm_expunges() {
        assert_eq!(delete_plan(false, true, true), DeletePlan::Expunge);
        assert_eq!(delete_plan(true, true, true), DeletePlan::Expunge);
    }

    #[test]
    fn plain_delete_inside_trash_refuses_and_explains() {
        // Moving Trash → Trash is a no-op; the only meaningful delete there is
        // permanent, and that must be asked for explicitly.
        assert_eq!(delete_plan(true, false, false), DeletePlan::RefuseInTrash);
    }
}
