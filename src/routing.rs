use bytes::Bytes;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashSet;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    OpenAI,
    Copilot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Responses,
    Compact,
    Lite,
}

pub struct RoutedRequest {
    pub provider: Provider,
    pub body: Bytes,
}

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("invalid combined Responses request")]
    InvalidRequest,
    #[error("unknown Copilot model alias")]
    UnknownCopilotModel,
    #[error("Copilot model catalog is unavailable")]
    CatalogUnavailable,
    #[error("Copilot does not support this Responses operation")]
    UnsupportedCopilotOperation,
}

#[derive(Deserialize)]
struct RequestModel<'a> {
    #[serde(borrow)]
    model: &'a RawValue,
}

#[derive(Deserialize)]
struct ModelName<'a>(#[serde(borrow)] Cow<'a, str>);

pub fn route_combined_request(
    body: Bytes,
    operation: Operation,
    copilot_models: Option<&HashSet<String>>,
) -> Result<RoutedRequest, RoutingError> {
    if body
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        != Some(b'{')
    {
        return Err(RoutingError::InvalidRequest);
    }
    let json = std::str::from_utf8(&body).map_err(|_| RoutingError::InvalidRequest)?;
    // Unknown fields are validated and skipped, not materialized or reserialized.
    // Deriving a struct also rejects duplicate model keys, including escaped keys.
    let request: RequestModel<'_> =
        serde_json::from_str(json).map_err(|_| RoutingError::InvalidRequest)?;
    let ModelName(model) = serde_json::from_str::<ModelName<'_>>(request.model.get())
        .map_err(|_| RoutingError::InvalidRequest)?;

    if !model.starts_with("copilot/") {
        drop(model);
        return Ok(RoutedRequest {
            provider: Provider::OpenAI,
            body,
        });
    }
    let copilot_models = copilot_models.ok_or(RoutingError::CatalogUnavailable)?;
    if model == "copilot/" || !copilot_models.contains(model.as_ref()) {
        return Err(RoutingError::UnknownCopilotModel);
    }
    if operation != Operation::Responses {
        return Err(RoutingError::UnsupportedCopilotOperation);
    }

    // RawValue borrows the exact JSON token. Verify its offset and containment
    // before replacing only that token; never search for matching prompt text.
    let raw_model = request.model.get().as_bytes();
    let start = (raw_model.as_ptr() as usize)
        .checked_sub(body.as_ptr() as usize)
        .ok_or(RoutingError::InvalidRequest)?;
    let end = start
        .checked_add(raw_model.len())
        .ok_or(RoutingError::InvalidRequest)?;
    if body.get(start..end) != Some(raw_model) {
        return Err(RoutingError::InvalidRequest);
    }
    let mut routed_body = Vec::with_capacity(body.len());
    routed_body.extend_from_slice(&body[..start]);
    serde_json::to_writer(&mut routed_body, &model["copilot/".len()..])
        .map_err(|_| RoutingError::InvalidRequest)?;
    routed_body.extend_from_slice(&body[end..]);

    Ok(RoutedRequest {
        provider: Provider::Copilot,
        body: Bytes::from(routed_body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> std::collections::HashSet<String> {
        [
            "copilot/gpt-6-astra",
            "copilot/gpt-5.6-sol",
            "copilot/example-model",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn new_catalog_aliases_route_without_code_changes_and_escape_only_model() {
        let aliases = std::collections::HashSet::from(["copilot/future-\"model".to_owned()]);
        let body = Bytes::from_static(
            br#" { "model" : "copilot/future-\"model", "opaque":1e400, "input":"unchanged" } "#,
        );
        let routed = route_combined_request(body, Operation::Responses, Some(&aliases)).unwrap();
        assert_eq!(routed.provider, Provider::Copilot);
        assert_eq!(
            routed.body.as_ref(),
            br#" { "model" : "future-\"model", "opaque":1e400, "input":"unchanged" } "#
        );
    }

    #[test]
    fn unavailable_catalog_denies_copilot_but_native_remains_zero_copy() {
        let native = Bytes::from_static(br#" { "model":"native", "opaque":1e400 } "#);
        let routed = route_combined_request(native.clone(), Operation::Responses, None).unwrap();
        assert_eq!(routed.provider, Provider::OpenAI);
        assert_eq!(routed.body.as_ptr(), native.as_ptr());
        assert!(matches!(
            route_combined_request(
                Bytes::from_static(br#"{"model":"copilot/gpt-6-astra"}"#),
                Operation::Responses,
                None,
            ),
            Err(RoutingError::CatalogUnavailable)
        ));
    }

    #[test]
    fn native_requests_keep_original_bytes_for_every_operation() {
        let body = Bytes::from_static(
            br#" { "model" : "gpt-6-astra", "input":[{"model":"copilot/gpt-6-astra","text":"keep \\ and \u0061"}], "large":1234567890123456789012345678901234567890, "exponent":1e400 } 
"#,
        );
        for operation in [Operation::Responses, Operation::Compact, Operation::Lite] {
            let routed = route_combined_request(body.clone(), operation, Some(&catalog())).unwrap();
            assert_eq!(routed.provider, Provider::OpenAI);
            assert_eq!(routed.body, body);
            assert_eq!(routed.body.as_ptr(), body.as_ptr());
        }
    }

    #[test]
    fn copilot_alias_replaces_only_the_top_level_raw_model_token() {
        let body = Bytes::from_static(
            br#" {"nested":{"model":"copilot/gpt-6-astra"}, "mo\u0064el" : "copilot\u002fgpt-6-astra", "input":"copilot/gpt-6-astra \\ \" \u0061", "large":1234567890123456789012345678901234567890, "exponent":1e400} 
"#,
        );
        let expected = br#" {"nested":{"model":"copilot/gpt-6-astra"}, "mo\u0064el" : "gpt-6-astra", "input":"copilot/gpt-6-astra \\ \" \u0061", "large":1234567890123456789012345678901234567890, "exponent":1e400} 
"#;

        let routed = route_combined_request(body, Operation::Responses, Some(&catalog())).unwrap();

        assert_eq!(routed.provider, Provider::Copilot);
        assert_eq!(routed.body.as_ref(), expected);
    }

    #[test]
    fn all_supported_copilot_aliases_remove_only_the_namespace() {
        for (body, expected) in [
            (
                r#"{"model":"copilot/gpt-6-astra"}"#,
                r#"{"model":"gpt-6-astra"}"#,
            ),
            (
                r#"{"model":"copilot/gpt-5.6-sol"}"#,
                r#"{"model":"gpt-5.6-sol"}"#,
            ),
            (
                r#"{"model":"copilot/example-model"}"#,
                r#"{"model":"example-model"}"#,
            ),
        ] {
            let routed = route_combined_request(
                Bytes::from_static(body.as_bytes()),
                Operation::Responses,
                Some(&catalog()),
            )
            .unwrap();
            assert_eq!(routed.provider, Provider::Copilot);
            assert_eq!(routed.body.as_ref(), expected.as_bytes());
        }
    }

    #[test]
    fn malformed_or_ambiguous_model_requests_are_rejected() {
        for body in [
            r#"{}"#,
            r#"{"input":{"model":"gpt-6-astra"}}"#,
            r#"{"model":null}"#,
            r#"{"model":6}"#,
            r#"{"model":{"name":"gpt-6-astra"}}"#,
            r#"{"model":["gpt-6-astra"]}"#,
            r#"{"model":"gpt-6-astra","model":"copilot/gpt-6-astra"}"#,
            r#"{"model":"gpt-6-astra","mo\u0064el":"gpt-6-astra"}"#,
            r#"{"model":"gpt-6-astra","broken":}"#,
            r#"{"model":"gpt-6-astra"} {}"#,
            r#"["gpt-6-astra"]"#,
            r#"{"model":"gpt-6-astra""#,
        ] {
            assert!(
                matches!(
                    route_combined_request(
                        Bytes::from_static(body.as_bytes()),
                        Operation::Responses,
                        Some(&catalog()),
                    ),
                    Err(RoutingError::InvalidRequest)
                ),
                "accepted invalid request: {body}"
            );
        }
    }

    #[test]
    fn invalid_utf8_in_ignored_fields_is_still_rejected() {
        let body = Bytes::from_static(b"{\"model\":\"gpt-6-astra\",\"input\":\"\xff\"}");

        assert!(matches!(
            route_combined_request(body, Operation::Responses, Some(&catalog())),
            Err(RoutingError::InvalidRequest)
        ));
    }

    #[test]
    fn unknown_copilot_aliases_never_fall_back_to_native() {
        for body in [
            r#"{"model":"copilot/"}"#,
            r#"{"model":"copilot/gpt-unknown"}"#,
            r#"{"model":"copilot/gpt-6-astra/extra"}"#,
            r#"{"model":"copilot\u002fgpt-unknown"}"#,
        ] {
            assert!(matches!(
                route_combined_request(
                    Bytes::from_static(body.as_bytes()),
                    Operation::Responses,
                    Some(&catalog())
                ),
                Err(RoutingError::UnknownCopilotModel)
            ));
        }
    }

    #[test]
    fn copilot_compaction_and_lite_never_fall_back_to_native() {
        for operation in [Operation::Compact, Operation::Lite] {
            assert!(matches!(
                route_combined_request(
                    Bytes::from_static(br#"{"model":"copilot/gpt-6-astra"}"#),
                    operation,
                    Some(&catalog()),
                ),
                Err(RoutingError::UnsupportedCopilotOperation)
            ));
        }
    }
}
