//! Bounded, additive Bkpr control protocol. Tokens and configuration are never serialized.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub protocol: u32,
    pub request_id: String,
    pub session_id: String,
    #[serde(default)]
    pub source_pane_id: Option<String>,
    #[serde(default)]
    pub client_id: Option<u16>,
    pub method: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub protocol: u32,
    pub request_id: String,
    pub outcome: String,
    pub result: Option<Value>,
    pub error: Option<ControlError>,
}

impl ControlResponse {
    pub fn completed(request: &ControlRequest, result: Value) -> Self {
        Self {
            protocol: PROTOCOL,
            request_id: request.request_id.clone(),
            outcome: "completed".into(),
            result: Some(result),
            error: None,
        }
    }
    pub fn failed(
        request: &ControlRequest,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            protocol: PROTOCOL,
            request_id: request.request_id.clone(),
            outcome: "failed".into(),
            result: None,
            error: Some(ControlError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
    pub fn unknown(request: &ControlRequest, message: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL,
            request_id: request.request_id.clone(),
            outcome: "unknown".into(),
            result: None,
            error: Some(ControlError {
                code: "completion_unknown".into(),
                message: message.into(),
            }),
        }
    }
}

/// Parse only native action argv. Root options, shell text and other commands
/// cannot escape this parser or become subprocess dispatch.
pub fn parse_action(argv: &[String]) -> Result<crate::cli::CliAction, String> {
    use clap::Parser;
    if argv.is_empty() || argv.len() > 4096 {
        return Err("action argv must contain between 1 and 4096 arguments".into());
    }
    let command = crate::cli::CliArgs::try_parse_from(
        ["zellij".to_owned(), "action".to_owned()]
            .into_iter()
            .chain(argv.iter().cloned()),
    )
    .map_err(|error| error.to_string())?;
    match command.command {
        Some(crate::cli::Command::Action(action)) => Ok(*action),
        _ => Err("expected a native CLI action".into()),
    }
}
