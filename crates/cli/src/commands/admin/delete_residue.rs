//! Inspect and conditionally discard legacy delete evidence.

use super::{get_admin_alias, recovery};
use crate::{exit_code::ExitCode, output::Formatter};
use clap::{Args, Subcommand};
use rc_core::Error;
use rc_core::admin::delete_residue::{
    DeleteResidueApi, DeleteResidueInspection, DeleteResidueTarget,
};
use rc_s3::admin::DeleteResidueClient;

#[derive(Debug, Subcommand)]
pub enum DeleteResidueCommands {
    /// List one bounded page of physical candidates; follow nextCursor on the same server
    List {
        alias: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..=64))]
        limit: u16,
    },
    /// Inspect the evidence for an exact object without changing it
    Inspect(ExactArgs),
    /// Discard eligible residue for an object explicitly intended to be deleted
    Discard {
        #[command(flatten)]
        exact: ExactArgs,
        /// Top-level fingerprint returned by inspection of this exact object
        #[arg(long)]
        expected_fingerprint: String,
    },
}

#[derive(Debug, Args)]
pub struct ExactArgs {
    alias: String,
    #[arg(long)]
    bucket: String,
    #[arg(long, allow_hyphen_values = true)]
    object: String,
    #[arg(long)]
    pool: usize,
    #[arg(long)]
    set: usize,
}

impl ExactArgs {
    fn target(self) -> DeleteResidueTarget {
        DeleteResidueTarget {
            bucket: self.bucket,
            object: self.object,
            pool: self.pool,
            set: self.set,
        }
    }
}

fn show_inspection(value: &DeleteResidueInspection, formatter: &Formatter) {
    formatter.println(&format!(
        "Bucket {:?}, object {:?}, pool {}, set {}, incarnation {}",
        value.target.bucket,
        value.target.object,
        value.target.pool,
        value.target.set,
        value.incarnation
    ));
    formatter.println(&format!(
        "Reason: {:?}; complete: {}; reclaimed: {}",
        value.reason, value.complete, value.reclaimed
    ));
    for disk in &value.participants {
        formatter.println(&format!(
            "Drive {}: {:?}, disk ID {:?}",
            disk.slot, disk.discovery, disk.disk_id
        ));
    }
    for txn in &value.transactions {
        formatter.println(&format!("Transaction {:?}", txn.id));
        for copy in &txn.copies {
            formatter.println(&format!(
                "  Drive {}: {:?}; current metadata {:?}, backup {:?}, journal {:?}",
                copy.slot,
                copy.state,
                copy.current_metadata_present,
                copy.backup_present,
                copy.journal_present
            ));
        }
    }
    if let Some(fingerprint) = &value.fingerprint {
        formatter.println(&format!("Discard fingerprint: {fingerprint}"));
    }
}

pub async fn execute(command: DeleteResidueCommands, formatter: &Formatter) -> ExitCode {
    let alias = match &command {
        DeleteResidueCommands::List { alias, .. } => alias,
        DeleteResidueCommands::Inspect(exact) | DeleteResidueCommands::Discard { exact, .. } => {
            &exact.alias
        }
    };
    let alias = match get_admin_alias(alias, formatter) {
        Ok(alias) => alias,
        Err(code) => return code,
    };
    let client = match DeleteResidueClient::new(&alias) {
        Ok(client) => client,
        Err(error) => return recovery::error("delete_residue", &error, false, formatter),
    };
    let (result, discard) = match command {
        DeleteResidueCommands::List {
            bucket,
            cursor,
            limit,
            ..
        } => {
            let page = match client.list(&bucket, cursor.as_deref(), limit).await {
                Ok(page) => page,
                Err(error) => return recovery::error("delete_residue", &error, false, formatter),
            };
            let partial = page.incomplete_sources > 0;
            let (status, message) = if partial {
                ("partial", "Some sources could not be inspected.")
            } else if page.complete {
                ("observed", "Bucket traversal complete.")
            } else {
                (
                    "observed",
                    "Bucket traversal continues; use the returned cursor.",
                )
            };
            recovery::output("delete_residue", status, message, &page, formatter);
            if !formatter.is_json() {
                formatter.println(&format!(
                    "Pool {}, set {}, drive {}: {:?}; incomplete sources: {}",
                    page.source.pool,
                    page.source.set,
                    page.source.drive,
                    page.source_status,
                    page.incomplete_sources
                ));
                for item in &page.candidates {
                    formatter.println(&format!("{:?}  {}", item.object, item.transaction));
                }
                if let Some(cursor) = &page.next_cursor {
                    formatter.println(&format!("Next cursor: {cursor}"));
                }
            }
            return if partial {
                ExitCode::GeneralError
            } else {
                ExitCode::Success
            };
        }
        DeleteResidueCommands::Inspect(exact) => (client.inspect(&exact.target()).await, false),
        DeleteResidueCommands::Discard {
            exact,
            expected_fingerprint,
        } => (
            client.discard(&exact.target(), &expected_fingerprint).await,
            true,
        ),
    };
    match result {
        Ok(value) => {
            let (status, message) = if discard {
                ("discarded", "Eligible legacy residue discarded.")
            } else if value.complete {
                ("observed", "Delete residue inspection complete.")
            } else {
                ("partial", "Delete residue inspection incomplete.")
            };
            recovery::output("delete_residue", status, message, &value, formatter);
            if !formatter.is_json() {
                show_inspection(&value, formatter);
            }
            if value.complete {
                ExitCode::Success
            } else {
                ExitCode::GeneralError
            }
        }
        Err(error) => {
            let unknown = discard
                && matches!(
                    &error,
                    Error::Network(_) | Error::General(_) | Error::Conflict(_) | Error::Io(_)
                );
            recovery::error("delete_residue", &error, unknown, formatter)
        }
    }
}
