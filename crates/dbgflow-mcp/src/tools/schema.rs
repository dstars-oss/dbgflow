use super::registry::{
    ToolDescriptor, ADD_SYMBOLS, CLOSE_SESSION, CREATE_SESSION, EVAL, GET_SESSION, LIST_SESSIONS,
    RECORD_PROFILE, RECORD_TTD,
};
use serde_json::json;

pub fn tool_descriptors() -> Vec<ToolDescriptor> {
    vec![
            ToolDescriptor {
                name: CREATE_SESSION,
                description:
                    "Create a debug session or return an existing session for the same target.",
                input_schema: json!({
                    "type": "object",
                    "description": "Example dump target: {\"target\":{\"kind\":\"dump\",\"path\":\"C:\\\\path\\\\file.dmp\"}}",
                    "properties": {
                        "target": {
                            "type": "object",
                            "description": "Debug target.",
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "dump" },
                                        "path": {
                                            "type": "string",
                                            "description": "Path to a local Windows dump file."
                                        }
                                    },
                                    "required": ["kind", "path"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "attach" },
                                        "pid": {
                                            "type": "integer",
                                            "minimum": 1,
                                            "description": "Process id to attach."
                                        }
                                    },
                                    "required": ["kind", "pid"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "launch" },
                                        "executable": {
                                            "type": "string",
                                            "description": "Path to a local executable."
                                        },
                                        "args": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Command-line arguments. Omit for no arguments."
                                        }
                                    },
                                    "required": ["kind", "executable"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    "required": ["target"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: GET_SESSION,
                description: "Get the current state of a debug session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by dbg.create_session."
                        }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: LIST_SESSIONS,
                description: "List debug sessions.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: CLOSE_SESSION,
                description: "Close a debug session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by dbg.create_session."
                        }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: EVAL,
                description: "Evaluate a native debugger command in a session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by dbg.create_session."
                        },
                        "command": {
                            "type": "string",
                            "description": "Native debugger command."
                        }
                    },
                    "required": ["session_id", "command"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: ADD_SYMBOLS,
                description: "Append native debugger symbol path entries to a session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by dbg.create_session."
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Debugger symbol path entries. Raw WinDbg symbol path strings are accepted."
                        }
                    },
                    "required": ["session_id", "paths"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: RECORD_PROFILE,
                description:
                    "Launch a process and record a native ETW profile trace as a standard ETL artifact.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "const": "launch" },
                                "executable": {
                                    "type": "string",
                                    "description": "Path to a local executable."
                                },
                                "args": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Command-line arguments. Omit for no arguments."
                                }
                            },
                            "required": ["kind", "executable"],
                            "additionalProperties": false
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Stop collection when the target exits or this timeout expires."
                        },
                        "collectors": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "kind": { "type": "string", "const": "native_etw" },
                                    "scope": {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "type": "string", "const": "target_process" }
                                        },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    "event_sets": {
                                        "type": "array",
                                        "items": { "type": "string", "enum": ["process", "file_io"] },
                                        "minItems": 1
                                    },
                                    "stacks": {
                                        "type": "object",
                                        "properties": {
                                            "enabled": { "type": "boolean" }
                                        },
                                        "additionalProperties": false
                                    }
                                },
                                "required": ["kind", "scope", "event_sets"],
                                "additionalProperties": false
                            },
                            "description": "Collectors to run around the same launched target. Omit to use native_etw target_process process and file_io with stacks enabled."
                        }
                    },
                    "required": ["target", "timeout_ms"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: RECORD_TTD,
                description:
                    "Record a Time Travel Debugging trace with TTD.exe. Supports launch, attach, and bounded monitor recording into controlled artifacts.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "launch" },
                                        "executable": {
                                            "type": "string",
                                            "description": "Path to a local executable."
                                        },
                                        "args": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Command-line arguments. Omit for no arguments."
                                        }
                                    },
                                    "required": ["kind", "executable"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "attach" },
                                        "pid": {
                                            "type": "integer",
                                            "minimum": 1,
                                            "description": "Process id to attach and record."
                                        }
                                    },
                                    "required": ["kind", "pid"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "monitor" },
                                        "program": {
                                            "type": "string",
                                            "description": "Executable file name or absolute executable path to monitor."
                                        },
                                        "cmd_line_filter": {
                                            "type": "string",
                                            "description": "Optional command-line substring filter for monitor mode."
                                        }
                                    },
                                    "required": ["kind", "program"],
                                    "additionalProperties": false
                                }
                            ]
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Stop recording when the recorder exits or this timeout expires."
                        },
                        "options": {
                            "type": "object",
                            "properties": {
                                "children": { "type": "boolean" },
                                "no_ui": { "type": "boolean" },
                                "accept_eula": { "type": "boolean" },
                                "ring": { "type": "boolean" },
                                "max_file_mb": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Maximum TTD trace size in MiB."
                                },
                                "modules": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "record_mode": {
                                    "type": "string",
                                    "enum": ["automatic", "manual"]
                                },
                                "replay_cpu_support": {
                                    "type": "string",
                                    "enum": [
                                        "default",
                                        "most_conservative",
                                        "most_aggressive",
                                        "intel_avx_required",
                                        "intel_avx2_required"
                                    ]
                                }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["target", "timeout_ms"],
                    "additionalProperties": false
                }),
            },
        ]
}
