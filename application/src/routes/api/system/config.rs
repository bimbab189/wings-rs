use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(crate) mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
    };

    const REDACTED: &str = "[REDACTED]";

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = serde_json::Value),
    ))]
    pub async fn route(state: GetState) -> ApiResponseResult {
        let mut config = match serde_json::to_value(&**state.config.load()) {
            Ok(config) => config,
            Err(error) => return ApiResponse::from(error).ok(),
        };
        redact_config(&mut config);
        ApiResponse::new_serialized(config).ok()
    }

    fn redact_config(value: &mut serde_json::Value) {
        let Some(object) = value.as_object_mut() else {
            return;
        };

        for (key, value) in object.iter_mut() {
            let normalized = key.to_ascii_lowercase();
            if normalized == "remote_headers" || normalized == "environment" {
                redact_map_values(value);
            } else if normalized == "registries" {
                redact_registry_values(value);
            } else if is_sensitive_key(&normalized) {
                *value = serde_json::Value::String(REDACTED.to_string());
            } else {
                redact_config(value);
            }
        }
    }

    fn redact_map_values(value: &mut serde_json::Value) {
        if let Some(values) = value.as_object_mut() {
            for value in values.values_mut() {
                *value = serde_json::Value::String(REDACTED.to_string());
            }
        } else {
            *value = serde_json::Value::String(REDACTED.to_string());
        }
    }

    fn redact_registry_values(value: &mut serde_json::Value) {
        let Some(registries) = value.as_object_mut() else {
            *value = serde_json::Value::String(REDACTED.to_string());
            return;
        };

        for registry in registries.values_mut() {
            if let Some(registry) = registry.as_object_mut() {
                for key in ["username", "password"] {
                    if registry.contains_key(key) {
                        registry.insert(
                            key.to_string(),
                            serde_json::Value::String(REDACTED.to_string()),
                        );
                    }
                }
            } else {
                *registry = serde_json::Value::String(REDACTED.to_string());
            }
        }
    }

    fn is_sensitive_key(key: &str) -> bool {
        key == "token"
            || key == "token_id"
            || key == "key"
            || key.contains("password")
            || key.contains("secret")
            || key.contains("credential")
            || key.contains("authorization")
            || key.contains("private_key")
            || key.contains("encryption_key")
    }

    #[cfg(test)]
    mod tests {
        use super::redact_config;

        #[test]
        fn redacts_credentials_in_nested_config_sections() {
            let mut config = serde_json::json!({
                "token_id": "test-token-id",
                "token": "test-token",
                "api": {"ssl": {"key": "test-private-key"}},
                "remote_headers": {"Authorization": "Bearer test-token"},
                "restic": {"environment": {"RESTIC_PASSWORD": "test-password"}},
                "registries": {
                    "registry.example": {
                        "username": "test-user",
                        "password": "test-password"
                    }
                },
                "app_name": "wings-rs"
            });

            redact_config(&mut config);

            let serialized = serde_json::to_string(&config).unwrap();
            assert!(!serialized.contains("test-token"));
            assert!(!serialized.contains("test-password"));
            assert!(!serialized.contains("test-private-key"));
            assert!(serialized.matches("[REDACTED]").count() >= 7);
            assert_eq!(config["app_name"], "wings-rs");
        }
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
