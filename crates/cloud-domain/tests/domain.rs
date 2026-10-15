#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use figcraft_cloud_domain::{
    CloudDocumentId, DeploymentMode, JobId, ObjectId, Permission, PrincipalId, ProjectId, QuotaLimit, QuotaValue, RequestId, ResourceState, Role,
    TenantId, Uuid, WorkspaceId,
};
use proptest::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

const ID: &str = "018f2f35-8dd8-7c25-8f3f-3a728cbd632a";

fn assert_json_round_trip<T>(value: T, expected: &str)
where
    T: Debug + DeserializeOwned + Eq + Serialize,
{
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(encoded, expected);
    assert_eq!(serde_json::from_str::<T>(&encoded).unwrap(), value);
}

#[test]
fn every_identifier_has_the_same_stable_json_shape() {
    macro_rules! assert_id {
        ($name:ident) => {
            assert_json_round_trip(ID.parse::<$name>().unwrap(), &format!("\"{ID}\""));
        };
    }

    assert_id!(TenantId);
    assert_id!(WorkspaceId);
    assert_id!(ProjectId);
    assert_id!(CloudDocumentId);
    assert_id!(PrincipalId);
    assert_id!(ObjectId);
    assert_id!(JobId);
    assert_id!(RequestId);
}

#[test]
fn identifiers_reject_nil_alternate_and_hostile_forms_without_echoing_them() {
    let invalid = [
        "",
        "00000000-0000-0000-0000-000000000000",
        "018F2F35-8DD8-7C25-8F3F-3A728CBD632A",
        "018f2f358dd87c258f3f3a728cbd632a",
        "{018f2f35-8dd8-7c25-8f3f-3a728cbd632a}",
        " 018f2f35-8dd8-7c25-8f3f-3a728cbd632a",
        "018f2f35-8dd8-7c25-8f3f-3a728cbd632a ",
        "018f2f35-8dd8-7c25-8f3f-3a728cbd632g",
        "secret-token-that-must-not-be-echoed",
    ];

    for input in invalid {
        let error = input.parse::<TenantId>().unwrap_err().to_string();
        assert!(input.is_empty() || !error.contains(input), "rejected input must not appear in its error");
        assert!(serde_json::from_str::<TenantId>(&format!("{input:?}")).is_err());
    }
}

#[test]
fn policy_values_have_explicit_wire_names_and_reject_wrong_shapes() {
    assert_json_round_trip(DeploymentMode::SingleTenant, "\"single\"");
    assert_json_round_trip(DeploymentMode::MultiTenant, "\"multi\"");
    assert_json_round_trip(DeploymentMode::Embedded, "\"embedded\"");
    assert_json_round_trip(Role::TeamManager, "\"team_manager\"");
    assert_json_round_trip(Permission::ManageProviderKeys, "\"manage_provider_keys\"");
    assert_json_round_trip(ResourceState::Pending, "\"pending\"");
    assert_json_round_trip(ResourceState::Active, "\"active\"");
    assert_json_round_trip(ResourceState::Suspended, "\"suspended\"");
    assert_json_round_trip(ResourceState::Archived, "\"archived\"");
    assert_json_round_trip(ResourceState::Deleting, "\"deleting\"");
    assert_json_round_trip(ResourceState::Deleted, "\"deleted\"");
    assert_json_round_trip(QuotaValue::new(42), "42");
    assert_json_round_trip(QuotaLimit::Unlimited, r#"{"kind":"unlimited"}"#);
    assert_json_round_trip(QuotaLimit::Limited(QuotaValue::new(42)), r#"{"kind":"limited","value":42}"#);

    assert!(serde_json::from_str::<DeploymentMode>("\"single_tenant\"").is_err());
    assert!(serde_json::from_str::<Role>("\"owner\"").is_err());
    assert!(serde_json::from_str::<Permission>("\"all\"").is_err());
    assert!(serde_json::from_str::<ResourceState>("\"enabled\"").is_err());
    assert!(serde_json::from_str::<QuotaValue>("-1").is_err());
    assert!(serde_json::from_str::<QuotaLimit>(r#"{"kind":"limited","value":-1}"#).is_err());
}

proptest! {
    #[test]
    fn any_non_nil_uuid_round_trips_through_each_identifier(bytes in any::<[u8; 16]>()) {
        let uuid = Uuid::from_bytes(bytes);
        if uuid.is_nil() {
            return Ok(());
        }

        macro_rules! round_trip {
            ($name:ident) => {{
                let id = $name::try_from(uuid).unwrap();
                let text = id.to_string();
                prop_assert_eq!(text.len(), 36);
                prop_assert_eq!(text.parse::<$name>().unwrap(), id);
                prop_assert_eq!(serde_json::from_str::<$name>(&serde_json::to_string(&id).unwrap()).unwrap(), id);
            }};
        }

        round_trip!(TenantId);
        round_trip!(WorkspaceId);
        round_trip!(ProjectId);
        round_trip!(CloudDocumentId);
        round_trip!(PrincipalId);
        round_trip!(ObjectId);
        round_trip!(JobId);
        round_trip!(RequestId);
    }

    #[test]
    fn arbitrary_text_never_panics_or_bypasses_canonical_form(input in any::<String>()) {
        if let Ok(id) = input.parse::<TenantId>() {
            prop_assert_eq!(id.to_string(), input);
            prop_assert!(!id.as_uuid().is_nil());
        }
    }
}
