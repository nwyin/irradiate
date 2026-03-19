use serde::{Deserialize, Serialize};

/// Messages sent from the orchestrator to a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorMessage {
    Warmup,
    Run {
        mutant: String,
        tests: Vec<String>,
        /// Per-mutant timeout in seconds. Used by the worker task to bound how long
        /// it waits for the Python worker to respond. Not consumed by the Python side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<f64>,
    },
    Shutdown,
}

/// Messages sent from a worker to the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Ready {
        pid: u32,
        #[serde(default)]
        tests: Vec<String>,
    },
    Result {
        mutant: String,
        exit_code: i32,
        duration: f64,
    },
    Error {
        #[serde(default)]
        mutant: Option<String>,
        message: String,
        #[serde(default)]
        duration: Option<f64>,
    },
}

/// A work item: which mutant to test, and which tests to run against it.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub mutant_name: String,
    pub test_ids: Vec<String>,
    /// Per-mutant timeout in seconds (multiplier × estimated test duration, floored at MIN).
    pub timeout_secs: f64,
}

/// Result of testing a single mutant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutantResult {
    pub mutant_name: String,
    pub exit_code: i32,
    pub duration: f64,
    pub status: MutantStatus,
}

/// Classification of a mutant test result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutantStatus {
    Survived,
    Killed,
    NoTests,
    TypeCheck,
    Timeout,
    Error,
}

impl MutantStatus {
    pub fn from_exit_code(exit_code: i32, timed_out: bool) -> Self {
        if timed_out {
            return MutantStatus::Timeout;
        }
        match exit_code {
            0 => MutantStatus::Survived,
            1 => MutantStatus::Killed,
            33 => MutantStatus::NoTests,
            37 => MutantStatus::TypeCheck,
            _ => MutantStatus::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestrator_message_serialization() {
        let msg = OrchestratorMessage::Run {
            mutant: "my_lib.x_hello__irradiate_1".to_string(),
            tests: vec!["tests/test.py::test_hello".to_string()],
            timeout_secs: Some(120.0),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"run\""));
        assert!(json.contains("my_lib.x_hello__irradiate_1"));
        assert!(json.contains("timeout_secs"));

        let parsed: OrchestratorMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            OrchestratorMessage::Run {
                mutant,
                tests,
                timeout_secs,
            } => {
                assert_eq!(mutant, "my_lib.x_hello__irradiate_1");
                assert_eq!(tests.len(), 1);
                assert_eq!(timeout_secs, Some(120.0));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_orchestrator_message_serialization_no_timeout() {
        // Run without timeout_secs: should not include the field in JSON (skip_serializing_if)
        let msg = OrchestratorMessage::Run {
            mutant: "mod.x_f__irradiate_1".to_string(),
            tests: vec![],
            timeout_secs: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("timeout_secs"),
            "None timeout_secs should be omitted from JSON"
        );

        // And deserializing old JSON (no timeout_secs field) should give None
        let old_json = r#"{"type":"run","mutant":"mod.x_f__irradiate_1","tests":[]}"#;
        let parsed: OrchestratorMessage = serde_json::from_str(old_json).unwrap();
        match parsed {
            OrchestratorMessage::Run { timeout_secs, .. } => {
                assert_eq!(
                    timeout_secs, None,
                    "missing field should deserialize as None"
                );
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_worker_message_deserialization() {
        let json = r#"{"type":"result","mutant":"m1","exit_code":1,"duration":0.042}"#;
        let msg: WorkerMessage = serde_json::from_str(json).unwrap();
        match msg {
            WorkerMessage::Result {
                exit_code,
                duration,
                ..
            } => {
                assert_eq!(exit_code, 1);
                assert!((duration - 0.042).abs() < 0.001);
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn test_mutant_status_from_exit_code() {
        assert_eq!(
            MutantStatus::from_exit_code(0, false),
            MutantStatus::Survived
        );
        assert_eq!(MutantStatus::from_exit_code(1, false), MutantStatus::Killed);
        assert_eq!(
            MutantStatus::from_exit_code(33, false),
            MutantStatus::NoTests
        );
        assert_eq!(
            MutantStatus::from_exit_code(37, false),
            MutantStatus::TypeCheck
        );
        assert_eq!(MutantStatus::from_exit_code(0, true), MutantStatus::Timeout);
        assert_eq!(MutantStatus::from_exit_code(2, false), MutantStatus::Error);
    }
}
