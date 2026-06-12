//! Access key inspection commands.
//!
//! Commands for resolving an access key to its IAM identity type and metadata.

use clap::Subcommand;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AccessKeyDetails, AccessKeyInfo, AdminApi, OpenIdAccessKeyInfo};

/// Access key inspection subcommands.
#[derive(Subcommand, Debug)]
pub enum AccessKeyCommands {
    /// Get access key information
    Info(InfoArgs),
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key to inspect
    pub access_key: String,
}

/// Execute an access key subcommand.
pub async fn execute(cmd: AccessKeyCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        AccessKeyCommands::Info(args) => execute_info(args, formatter).await,
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_access_key_info(&args.access_key).await {
        Ok(info) => {
            if formatter.is_json() {
                formatter.json(&info);
            } else {
                print_access_key_info(&info, formatter);
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Access key '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) if is_access_key_not_found_error(&e) => {
            formatter.error(&format!("Access key '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to get access key info: {e}"),
        ),
    }
}

fn is_access_key_not_found_error(error: &rc_core::Error) -> bool {
    error.to_string().contains("access key not exist")
}

fn print_access_key_info(info: &AccessKeyInfo, formatter: &Formatter) {
    let styled_key = formatter.style_name(&info.access_key);
    formatter.println(&format!("Access Key:    {styled_key}"));
    formatter.println(&format!("User Type:     {}", info.user_type));
    formatter.println(&format!("Provider:      {}", info.user_provider));

    print_common_info(&info.info, formatter);

    if let Some(username) = &info.ldap_specific_info.username {
        formatter.println(&format!("LDAP Username: {username}"));
    }

    print_openid_info(&info.open_id_specific_info, formatter);
}

fn print_common_info(info: &AccessKeyDetails, formatter: &Formatter) {
    if let Some(parent) = &info.parent_user {
        formatter.println(&format!("Parent User:   {parent}"));
    }

    if let Some(status) = &info.account_status {
        formatter.println(&format!("Status:        {status}"));
    }

    if let Some(expiration) = &info.expiration {
        formatter.println(&format!("Expiration:    {expiration}"));
    }

    if let Some(name) = &info.name {
        formatter.println(&format!("Name:          {name}"));
    }

    if let Some(description) = &info.description {
        formatter.println(&format!("Description:   {description}"));
    }

    if let Some(implied_policy) = info.implied_policy {
        formatter.println(&format!("Implied Policy: {implied_policy}"));
    }

    if let Some(policy) = &info.policy {
        formatter.println("");
        formatter.println("Policy:");
        formatter.println(policy);
    }
}

fn print_openid_info(info: &OpenIdAccessKeyInfo, formatter: &Formatter) {
    if let Some(config_name) = &info.config_name {
        formatter.println(&format!("OpenID Config: {config_name}"));
    }

    if let Some(user_id) = &info.user_id {
        formatter.println(&format!("OpenID User:   {user_id}"));
    }

    if let Some(user_id_claim) = &info.user_id_claim {
        formatter.println(&format!("User Claim:    {user_id_claim}"));
    }

    if let Some(display_name) = &info.display_name {
        formatter.println(&format!("Display Name:  {display_name}"));
    }

    if let Some(display_name_claim) = &info.display_name_claim {
        formatter.println(&format!("Display Claim: {display_name_claim}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::admin::{AccessKeyDetails, LdapAccessKeyInfo, OpenIdAccessKeyInfo};

    #[test]
    fn test_access_key_info_serializes_server_shape() {
        let info = AccessKeyInfo {
            access_key: "svc-ldap".to_string(),
            user_type: "Service Account".to_string(),
            user_provider: "ldap".to_string(),
            info: AccessKeyDetails {
                parent_user: Some("ldap-parent".to_string()),
                account_status: Some("on".to_string()),
                implied_policy: Some(false),
                policy: Some("{\"Version\":\"2012-10-17\"}".to_string()),
                expiration: None,
                name: Some("LDAP Service".to_string()),
                description: None,
            },
            ldap_specific_info: LdapAccessKeyInfo {
                username: Some("alice".to_string()),
            },
            open_id_specific_info: OpenIdAccessKeyInfo::default(),
        };

        let value = serde_json::to_value(info).expect("serialize access key info");

        assert_eq!(value["accessKey"], "svc-ldap");
        assert_eq!(value["userType"], "Service Account");
        assert_eq!(value["userProvider"], "ldap");
        assert_eq!(value["parentUser"], "ldap-parent");
        assert_eq!(value["accountStatus"], "on");
        assert_eq!(value["ldapSpecificInfo"]["username"], "alice");
        assert!(value.get("openIDSpecificInfo").is_none());
        assert!(value.get("openIdSpecificInfo").is_none());
    }

    #[test]
    fn test_access_key_not_exist_error_maps_to_not_found() {
        let error = rc_core::Error::General("Bad request: access key not exist".to_string());

        assert!(is_access_key_not_found_error(&error));
    }
}
