use super::protocol::*;
use crate::agent::session::SessionId;
use serde_json::json;
use uuid::Uuid;

fn manifest() -> EnvironmentManifest {
    EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/project".into(),
        tools: vec![ToolDescriptor {
            id: "exec".into(),
            kind: ToolKind::Exec,
            description: "Local shell".into(),
            parameters: json!({"type":"object"}),
            deferred: false,
        }],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    }
}

fn key() -> EnvironmentKey {
    EnvironmentKey {
        connection_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
    }
}

#[test]
fn client_frames_roundtrip_with_version_and_strict_fields() {
    let frame = ClientFrame::Hello {
        session_id: SessionId::new("session"),
        manifest: manifest(),
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded: ClientFrame = decode_frame(&bytes).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap(),
        serde_json::to_value(frame).unwrap()
    );
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["version"] = json!(2);
    assert!(decode_frame::<ClientFrame>(&serde_json::to_vec(&wire).unwrap()).is_err());
    wire["version"] = json!(1);
    wire["frame"]["unexpected"] = json!(true);
    assert!(decode_frame::<ClientFrame>(&serde_json::to_vec(&wire).unwrap()).is_err());
    wire["frame"].as_object_mut().unwrap().remove("unexpected");
    wire["extra"] = json!(true);
    assert!(decode_frame::<ClientFrame>(&serde_json::to_vec(&wire).unwrap()).is_err());
}

#[test]
fn server_frames_preserve_generation_and_call_identity() {
    let frame = ServerFrame::Invoke {
        invocation: Invocation {
            environment: key(),
            session_id: SessionId::new("session"),
            run_id: "run".into(),
            call_id: "call".into(),
            tool_id: "exec".into(),
            arguments: json!({"cmd":"pwd"}),
            deadline: chrono::Utc::now(),
        },
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded: ServerFrame = decode_frame(&bytes).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap(),
        serde_json::to_value(frame).unwrap()
    );
}

#[test]
fn canonical_arguments_hash_covers_nested_objects_and_array_order() {
    let a: serde_json::Value = serde_json::from_str(r#"{"b":{"y":2,"x":1},"a":[1,2]}"#).unwrap();
    let b = json!({"a":[1,2],"b":{"x":1,"y":2}});
    let hash = arguments_hash(&a).unwrap();
    assert_eq!(hash.len(), 64);
    assert_eq!(hash, arguments_hash(&b).unwrap());
    assert_ne!(
        hash,
        arguments_hash(&json!({"a":[2,1],"b":{"x":1,"y":2}})).unwrap()
    );
    assert_ne!(
        hash,
        arguments_hash(&json!({"a":[1,2],"b":{"x":1,"y":3}})).unwrap()
    );
    assert_eq!(
        arguments_hash(&json!({})).unwrap(),
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

#[test]
fn manifest_rejects_invalid_duplicate_or_excessive_tools() {
    let mut value = manifest();
    assert!(validate_manifest(&value).is_ok());
    for id in ["", "инструмент", "path/to/tool", &"a".repeat(65)] {
        value.tools[0].id = id.into();
        assert!(validate_manifest(&value).is_err(), "accepted {id}");
    }
    value = manifest();
    value.tools.push(value.tools[0].clone());
    assert!(validate_manifest(&value).is_err());
    value.tools = (0..257)
        .map(|i| ToolDescriptor {
            id: format!("tool_{i}"),
            ..manifest().tools.remove(0)
        })
        .collect();
    assert!(validate_manifest(&value).is_err());
}

#[test]
fn manifest_and_project_instructions_have_byte_limits() {
    let mut value = manifest();
    value.project_instructions = Some(ProjectInstructions {
        path: "/project/AGENTS.md".into(),
        content: "a".repeat(64 * 1024),
        content_hash: "a".repeat(64),
    });
    assert!(validate_manifest(&value).is_ok());
    value
        .project_instructions
        .as_mut()
        .unwrap()
        .content
        .push('a');
    assert!(validate_manifest(&value).is_err());
    value = manifest();
    value.label = "x".repeat(256 * 1024);
    assert!(validate_manifest(&value).is_err());
    value = manifest();
    value.skills = (0..65)
        .map(|i| SkillDescriptor {
            id: format!("skill_{i}"),
            name: format!("skill-{i}"),
            description: "test".into(),
            path: format!("/skills/{i}/SKILL.md"),
            content_hash: "a".repeat(64),
        })
        .collect();
    assert!(validate_manifest(&value).is_err());
}

#[test]
fn frame_and_chunk_limits_apply_before_transport() {
    assert!(decode_frame::<ClientFrame>(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    let frame = ClientFrame::ResultChunk {
        environment: key(),
        run_id: "run".into(),
        call_id: "call".into(),
        arguments_hash: "a".repeat(64),
        sequence: 0,
        final_chunk: false,
        data: vec![b'a'; MAX_CHUNK_BYTES + 1],
    };
    assert!(encode_frame(&frame).is_err());
    // Low-valued bytes expand in JSON too: the encoded frame bound also matters.
    let oversized = ClientFrame::ResultChunk {
        environment: key(),
        run_id: "run".into(),
        call_id: "call".into(),
        arguments_hash: "a".repeat(64),
        sequence: 0,
        final_chunk: true,
        data: vec![0; MAX_CHUNK_BYTES],
    };
    assert!(encode_frame(&oversized).is_ok());
}
