use super::*;
use crate::commands::admin::recovery::test_support::*;

fn target() -> TargetArgs {
    TargetArgs {
        alias: "local".into(),
        node_id: NODE.into(),
        responsibility_id: ID.into(),
    }
}

fn page() -> PageArgs {
    PageArgs {
        alias: "local".into(),
        cursor: Some("cursor".into()),
    }
}

fn summary() -> MrfReply {
    MrfReply::Summary {
        summary: serde_json::from_value(serde_json::json!({
            "contractVersion":1, "sampledAtMs":10, "checkpointReadable":true,
            "checkpointOwner":null, "checkpointSequence":null, "activeResponsibilityCount":0,
            "heldCount":1, "legacyGenerationUnknownCount":0, "bucketIncarnationChangedCount":0,
            "operatorAcceptedCount":1, "bestEffortQueueDepth":0, "responsibilityBytes":512
        }))
        .expect("summary fixture"),
    }
}

fn actions() -> Vec<MrfCommands> {
    vec![
        MrfCommands::Refresh(RefreshArgs {
            target: target(),
            expected_bucket_incarnation: Some(INCARNATION.into()),
        }),
        MrfCommands::Recheck(RecheckArgs {
            target: target(),
            expected_bucket_incarnation: INCARNATION.into(),
        }),
        MrfCommands::AcceptRisk(AcceptArgs {
            target: RecheckArgs {
                target: target(),
                expected_bucket_incarnation: INCARNATION.into(),
            },
            request_id: REQUEST_ID.into(),
            reason: "Operator decision".into(),
            reference: "CASE-3177".into(),
            acknowledge_unverified_data_risk: true,
            acknowledge_unknown_source_incarnation: false,
            acknowledge_bucket_incarnation_mismatch: false,
        }),
    ]
}

#[tokio::test]
async fn status_and_list_keep_complete_partial_and_unsupported_exit_codes_distinct() {
    for command in [
        MrfCommands::Status(page()),
        MrfCommands::List(ListArgs {
            page: page(),
            limit: 32,
        }),
    ] {
        for (state, expected) in [
            ("complete", ExitCode::Success),
            ("partial", ExitCode::GeneralError),
            ("unsupported", ExitCode::UnsupportedFeature),
        ] {
            let api = MockApi::default();
            let reply = match &command {
                MrfCommands::List(_) => MrfReply::List {
                    snapshot: MrfPage {
                        responsibilities: vec![row()],
                        next_cursor: None,
                        metadata: Default::default(),
                    },
                    observed_at_ms: 20,
                },
                _ => summary(),
            };
            let mut response = observation(reply);
            if state == "partial" {
                response.next_cursor = Some("next".into());
            }
            if state == "unsupported" {
                response.nodes[0].state = "unsupported".into();
                response.nodes[0].reply = None;
            }
            api.observations.lock().unwrap().push_back(Ok(response));
            assert_eq!(
                execute_with_api(&command, &api, &formatter()).await,
                expected,
                "{command:?}: {state}"
            );
            let calls = api.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "no automatic pagination");
            assert_eq!(
                calls[0],
                match command {
                    MrfCommands::Status(_) => Call::Summary(Some("cursor".into())),
                    _ => Call::List(Some("cursor".into()), 32),
                }
            );
        }
    }
}

#[tokio::test]
async fn get_reports_absence_as_unconfirmed_and_preserves_auth_error() {
    for (responsibility, expected) in [
        (Some(row()), ExitCode::Success),
        (None, ExitCode::GeneralError),
    ] {
        let api = MockApi::default();
        api.observations
            .lock()
            .unwrap()
            .push_back(Ok(observation(MrfReply::Get {
                observation: MrfCurrentResponsibility {
                    contract_version: 1,
                    observed_at_ms: 20,
                    responsibility,
                },
            })));
        assert_eq!(
            execute_with_api(&MrfCommands::Get(target()), &api, &formatter()).await,
            expected
        );
        assert_eq!(
            *api.calls.lock().unwrap(),
            vec![Call::Get(NODE.into(), ID.into())]
        );
    }
    let api = MockApi::default();
    api.observations
        .lock()
        .unwrap()
        .push_back(Err(Error::Auth("denied".into())));
    assert_eq!(
        execute_with_api(&MrfCommands::Get(target()), &api, &formatter()).await,
        ExitCode::AuthError
    );
}

#[tokio::test]
async fn every_action_requires_matching_retained_unverified_receipt() {
    for command in actions() {
        let action = command.action().unwrap().unwrap();
        let state = match action.request.action.as_str() {
            "refresh" => "refreshed",
            "recheck" => "active",
            _ => "operatorAcceptedUnverified",
        };
        for (verified, expected) in [(false, ExitCode::Success), (true, ExitCode::GeneralError)] {
            let api = MockApi::default();
            api.observations
                .lock()
                .unwrap()
                .push_back(Ok(observation(MrfReply::Action {
                    action: MrfActionResult {
                        responsibility_id: ID.into(),
                        state: state.into(),
                        data_verified: verified,
                        durable_responsibility_retained: true,
                    },
                })));
            assert_eq!(
                execute_with_api(&command, &api, &formatter()).await,
                expected
            );
            assert_eq!(
                *api.calls.lock().unwrap(),
                vec![Call::Action(serde_json::to_value(&action).unwrap())]
            );
        }
        let api = MockApi::default();
        api.observations
            .lock()
            .unwrap()
            .push_back(Err(Error::Network("ack lost".into())));
        assert_eq!(
            execute_with_api(&command, &api, &formatter()).await,
            ExitCode::GeneralError
        );
        assert_eq!(
            api.calls.lock().unwrap().len(),
            1,
            "ambiguous actions must not retry"
        );
    }
}

#[tokio::test]
async fn risk_acceptance_invalid_acknowledgment_does_not_call_api() {
    let MrfCommands::AcceptRisk(mut args) = actions().pop().unwrap() else {
        panic!("risk action fixture")
    };
    args.acknowledge_unverified_data_risk = false;
    let api = MockApi::default();
    assert_eq!(
        execute_with_api(&MrfCommands::AcceptRisk(args), &api, &formatter()).await,
        ExitCode::UsageError
    );
    assert!(api.calls.lock().unwrap().is_empty());
}
