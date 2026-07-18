fn main() {
    kigi_proto_build::configure()
        .type_attribute(
            ".", // match every message & enum
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        // ToolConfigEntry is embedded in external JSON contracts
        // (agent-config JSON) where sparse payloads must deserialize. Defaults are applied per optional field (not
        // type-level) so the required `id` field still fails deserialization
        // when missing instead of silently becoming "". See tests/wire_shape.rs.
        .field_attribute(
            ".kigi.tools.v1.ToolConfigEntry.params_json",
            "#[serde(default)]",
        )
        .field_attribute(
            ".kigi.tools.v1.ToolConfigEntry.name_override",
            "#[serde(default)]",
        )
        .field_attribute(
            ".kigi.tools.v1.ToolConfigEntry.params_name_overrides",
            "#[serde(default)]",
        )
        .field_attribute(
            ".kigi.tools.v1.ToolConfigEntry.behavior_version",
            "#[serde(default)]",
        )
        .field_attribute(
            ".kigi.tools.v1.ToolConfigEntry.description_override",
            "#[serde(default)]",
        )
        .compile_protos(&["proto/kigi-tools.proto"], &["proto/"])
        .unwrap();
}
