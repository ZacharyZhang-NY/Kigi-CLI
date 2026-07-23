use agent_client_protocol as acp;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Info {
    pub id: acp::SessionId,
    pub cwd: String,
}
