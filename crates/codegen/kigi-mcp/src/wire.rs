//! Single source of truth for the `kigi/mcp/*` ACP wire strings.
//!
//! These method/`_meta` keys are part of the cross-language MCP-over-ACP
//! protocol the SDK speaks (mirrors the SDK's `_mcp_wire.py` / `mcpWire.ts`).
//! Reference these constants instead of re-typing the literals so the agent and
//! SDK can't drift apart.

/// Forward tool invocation (client -> agent): the pager/client asks the agent to
/// invoke a tool on an MCP server the agent is connected to, outside the LLM loop.
/// See `extensions::mcp::handle_call`.
pub const MCP_CALL: &str = "kigi/mcp/call";

/// Reverse zero-IPC tool invocation (agent -> client): the agent invokes a tool
/// living in the SDK's in-process MCP server by sending the MCP JSON-RPC message
/// back over the ACP reverse channel. Distinct from [`MCP_CALL`] so the two
/// disjoint schemas don't share a method string for metrics/tracing.
pub const MCP_SDK_CALL: &str = "kigi/mcp/sdk_call";

/// `session/new` `_meta` key listing in-process SDK MCP servers.
pub const MCP_SERVERS: &str = "kigi/mcp/servers";

/// `initialize` `_meta` capability flag advertising in-process SDK MCP support,
/// which enables the SDK's `transport="acp"`.
pub const MCP_SDK: &str = "kigi/mcp/sdk";
