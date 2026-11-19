//! Standalone source-accounting fixtures, independent of gateway integration.
#[path = "../reporting/usage.rs"]
mod reporting;
mod upstream {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Format {
        OpenAi,
        Claude,
        Gemini,
    }
}
#[path = "usage.rs"]
mod usage;
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn source_semantics() {
        for (format, value, input, output, reasoning) in [
            (
                upstream::Format::OpenAi,
                json!({"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}),
                10,
                4,
                2,
            ),
            (
                upstream::Format::Claude,
                json!({"usage":{"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":4,"thinking_tokens":2}}),
                15,
                4,
                2,
            ),
            (
                upstream::Format::Gemini,
                json!({"usageMetadata":{"promptTokenCount":10,"toolUsePromptTokenCount":2,"candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":18}}),
                12,
                6,
                2,
            ),
        ] {
            let mut usage = usage::Usage::new(format);
            usage.json(&value);
            let b = usage.breakdown();
            assert!(b.valid());
            assert_eq!(b.quality, "complete");
            assert_eq!(
                (
                    b.input.total_tokens,
                    b.output.total_tokens,
                    b.output.reasoning_tokens
                ),
                (input, output, reasoning)
            );
        }
    }
    #[test]
    fn partial_and_explicit_zero() {
        let mut u = usage::Usage::new(upstream::Format::OpenAi);
        u.json(&json!({"usage":{"input_tokens":10,"total_tokens":15}}));
        assert_eq!(u.breakdown().unclassified_tokens, 5);
        u.json(&json!({"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}));
        assert_eq!(u.breakdown().quality, "complete");
        let mut u = usage::Usage::new(upstream::Format::OpenAi);
        u.json(&json!({"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":0}}));
        assert_eq!(u.breakdown().total_tokens, 0);
        assert_eq!(u.breakdown().quality, "inconsistent");
    }
    #[test]
    fn fragmented_claude_usage_merges() {
        let mut u = usage::Usage::new(upstream::Format::Claude);
        for byte in b"data: {\"message\":{\"usage\":{\"input_tokens\":8,\"cache_read_input_tokens\":3}}}\r\n\r\ndata: {\"usage\":{\"output_tokens\":5}}\n\n" { u.feed(&[*byte]); }
        assert_eq!(u.breakdown().total_tokens, 16);
        assert_eq!(u.breakdown().input.cache_read_tokens, 3);
    }
}
