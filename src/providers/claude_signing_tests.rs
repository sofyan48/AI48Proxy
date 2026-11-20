use super::{billing_offset, normalize, sign};

// CLIProxyAPI/internal/runtime/executor/claude_signing_test.go (Claude Code 2.1.220).
const BASE: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

#[test]
fn go_known_vectors() {
    let vectors = [
        ("base", BASE.to_owned(), "7ee87"),
        ("model", BASE.replacen("model-a", "model-b", 1), "7ee87"),
        ("max tokens", BASE.replacen(r#""max_tokens":1"#, r#""max_tokens":2"#, 1), "7ee87"),
        ("message", BASE.replacen(r#""text":"x""#, r#""text":"y""#, 1), "b9cc8"),
        ("system", BASE.replacen("system-x", "system-y", 1), "a30d3"),
        ("metadata", BASE.replacen("meta-x", "meta-y", 1), "7a89d"),
        ("thinking", BASE.replacen(r#""thinking":{"type":"adaptive","display":"omitted"}"#, r#""thinking":{"type":"disabled"}"#, 1), "7205c"),
        ("context", BASE.replacen(r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#, r#""context_management":{"edits":[]}"#, 1), "05073"),
        ("effort", BASE.replacen(r#""effort":"high""#, r#""effort":"low""#, 1), "12366"),
        ("stream", BASE.replacen(r#""stream":true"#, r#""stream":false"#, 1), "60400"),
        ("tools", BASE.replacen(r#""tools":[]"#, r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#, 1), "3d78d"),
        ("extra field", BASE.replacen(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#, 1), "2d622"),
        ("field order", r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#.to_owned(), "e5b6c"),
        ("nested model", metadata(r#""model":"a""#), "0601b"),
        ("nested max tokens", metadata(r#""max_tokens":2"#), "7ee87"),
        ("top fallbacks", BASE.replacen(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#, 1), "7ee87"),
        ("nested fallbacks", metadata(r#""fallbacks":[{"model":"nested-a"}]"#), "7ee87"),
        ("top credit token", BASE.replacen(r#""stream":true}"#, r#""stream":true,"fallback_credit_token":"a"}"#, 1), "7ee87"),
        ("nested credit token", metadata(r#""fallback_credit_token":"a""#), "7ee87"),
        ("trailing dispatch run", metadata(r#""max_tokens":999,"fallbacks":[{"model":"fallback-model"}]"#), "4589b"),
        ("model before dispatch run", metadata(r#""model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token""#), "2d312"),
        ("model splits dispatch runs", metadata(r#""max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]"#), "0601b"),
        ("ordinary nested member", metadata(r#""plain":"a""#), "8d74c"),
        ("billing only", r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_owned(), "f2edb"),
    ];
    for (name, body, expected) in vectors {
        let signed = sign(body.as_bytes().to_vec()).unwrap();
        let offset = billing_offset(&signed).unwrap();
        assert_eq!(&signed[offset..offset + 5], expected.as_bytes(), "{name}");
        let mut restored = signed.clone();
        restored[offset..offset + 5].copy_from_slice(b"00000");
        assert_eq!(restored, body.as_bytes(), "changed non-CCH bytes: {name}");
        assert_eq!(
            sign(signed.clone()).unwrap(),
            signed,
            "not idempotent: {name}"
        );
    }
}

fn metadata(extra: &str) -> String {
    BASE.replacen(
        r#""metadata":{"user_id":"meta-x"}"#,
        &format!(r#""metadata":{{"user_id":"meta-x",{extra}}}"#),
        1,
    )
}

#[test]
fn go_raw_normalization_vectors() {
    let vectors = [
        (r#"{"model":"claude","keep":1}"#, r#"{"model":"","keep":1}"#),
        (r#"{"max_tokens":1,"keep":2}"#, r#"{"keep":2}"#),
        (
            r#"{"keep":1,"fallbacks":[{"model":"x"}],"tail":2}"#,
            r#"{"keep":1,"tail":2}"#,
        ),
        (
            r#"{"keep":1,"fallback_credit_token":"secret"}"#,
            r#"{"keep":1}"#,
        ),
        (
            r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"secret"}"#,
            "{}",
        ),
        (
            r#"{"keep":1,"max_tokens":1,"fallbacks":[],"tail":2}"#,
            r#"{"keep":1,"tail":2}"#,
        ),
        (
            r#"{"keep":1,"max_tokens":1,"fallbacks":[]}"#,
            r#"{"keep":1,}"#,
        ),
        (
            r#"{"outer":{"model":"x","max_tokens":1,"keep":"y"}}"#,
            r#"{"outer":{"model":"","keep":"y"}}"#,
        ),
        (
            r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
            r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
        ),
        // Raw escaped keys are not normalized by Go, and non-string models stay intact.
        (
            r#"{"mo\u0064el":"keep","max_\u0074okens":2,"model":42}"#,
            r#"{"mo\u0064el":"keep","max_\u0074okens":2,"model":42}"#,
        ),
        (
            r#"[{"model":"a\\\"b","max_tokens":{"model":"ignored"}},{"model":null}]"#,
            r#"[{"model":""},{"model":null}]"#,
        ),
        (
            " { \"keep\" : 1 , \"max_tokens\": 2 , \"fallbacks\": [] } \n",
            " { \"keep\" : 1 ,  } \n",
        ),
        (
            "{ \"max_tokens\":1 , \"keep\":1e+03, \"model\" : \"x\" }",
            "{  \"keep\":1e+03, \"model\" : \"\" }",
        ),
    ];
    for (body, expected) in vectors {
        assert_eq!(
            normalize(body.as_bytes()).unwrap(),
            expected.as_bytes(),
            "{body}"
        );
    }
}

#[test]
fn absent_or_ineligible_header_is_byte_identical() {
    for body in [
        "not JSON",
        "",
        "{}",
        "[]",
        r#"{"system":"x-anthropic-billing-header: cch=00000;"}"#,
        r#"{"system":[{"text":"ordinary cch=00000;"}]}"#,
        r#"{"system":[{"text":" x-anthropic-billing-header: cch=00000;"}]}"#,
        r#"{"system":[{"text":"ordinary"},{"text":"x-anthropic-billing-header: cch=00000;"}]}"#,
        r#"{"system":[{"text":null},{"text":"x-anthropic-billing-header: cch=00000;"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: no signature;"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: cch=ABCDE;"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: cch=1234;"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: cch=123456;"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: cch=12345"}]}"#,
        r#"{"system":[{"text":"x-anthropic-billing-header: cch=\u00300000;"}]}"#,
    ] {
        assert_eq!(
            sign(body.as_bytes().to_vec()).unwrap(),
            body.as_bytes(),
            "{body}"
        );
    }
}

#[test]
fn preserves_whitespace_escapes_unicode_and_other_cch_literals() {
    let body = " {\n  \"model\": \"原始\\u002dmodel\", \"max_tokens\": 99,\n  \"messages\": [{\"content\":\"literal cch=00000; \\u003c & café\"}],\n  \"system\": [{\"text\":\"x-anthropic-billing-header: cch=abcde; cch=12345;\"}, {\"text\":\"cch=00000;\"}]\n}\n";
    let signed = sign(body.as_bytes().to_vec()).unwrap();
    let offset = billing_offset(&signed).unwrap();
    let mut restored = signed.clone();
    restored[offset..offset + 5].copy_from_slice(b"abcde");
    assert_eq!(restored, body.as_bytes());
    assert_eq!(sign(signed.clone()).unwrap(), signed);
}

#[test]
fn searches_past_invalid_cch_and_handles_decoded_billing_prefix() {
    let body = r#"{"system":[{"text":"\u0078-anthropic-billing-header: cch=ABCDE; cch=abcde;"}]}"#;
    let offset = billing_offset(body.as_bytes()).unwrap();
    assert_eq!(&body.as_bytes()[offset..offset + 5], b"abcde");
    let signed = sign(body.as_bytes().to_vec()).unwrap();
    assert_eq!(&signed[..offset], &body.as_bytes()[..offset]);
    assert_eq!(&signed[offset + 5..], &body.as_bytes()[offset + 5..]);
}

#[test]
fn invalid_eligible_body_returns_error() {
    for body in [
        format!("{BASE} trailing"),
        BASE.replacen(r#""max_tokens":1"#, r#""max_tokens":NaN"#, 1),
    ] {
        assert!(sign(body.into_bytes()).is_err());
    }
    for body in ["{", "{\"a\":}", "[1,]", "{}{}", "{\"x\":01}"] {
        assert!(normalize(body.as_bytes()).is_err(), "{body}");
    }
}
