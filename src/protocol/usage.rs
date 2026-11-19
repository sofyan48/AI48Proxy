//! Downstream usage conversion; source accounting is performed before translation.
use super::*;

pub(super) fn mapped_usage(from: Format, u: &Value) -> Result<Value> {
    let mut result = json!({});
    let read = if from == Format::OpenAiChat {
        optional(u.pointer("/prompt_tokens_details/cached_tokens"))?
    } else {
        optional(u.get("cache_read_input_tokens"))?
    };
    let write = if from == Format::OpenAiChat {
        optional(u.pointer("/prompt_tokens_details/cache_creation_tokens"))?
    } else {
        optional(u.get("cache_creation_input_tokens"))?
    };
    let input = optional(u.get(if from == Format::OpenAiChat {
        "prompt_tokens"
    } else {
        "input_tokens"
    }))?;
    let output = optional(u.get(if from == Format::OpenAiChat {
        "completion_tokens"
    } else {
        "output_tokens"
    }))?;
    let reasoning = if from == Format::OpenAiChat {
        optional(u.pointer("/completion_tokens_details/reasoning_tokens"))?
    } else {
        optional(
            u.pointer("/output_tokens_details/thinking_tokens")
                .or_else(|| u.pointer("/output_tokens_details/reasoning_tokens"))
                .or_else(|| u.get("thinking_tokens")),
        )?
    };
    let cache = read
        .unwrap_or(0)
        .checked_add(write.unwrap_or(0))
        .ok_or_else(|| invalid("cache overflow"))?;
    let input = input
        .map(|i| {
            if from == Format::OpenAiChat {
                i.checked_sub(cache)
            } else {
                i.checked_add(cache)
            }
        })
        .transpose_count()?;
    if let Some(input) = input {
        result[if from == Format::OpenAiChat {
            "input_tokens"
        } else {
            "prompt_tokens"
        }] = json!(input);
    }
    if let Some(output) = output {
        result[if from == Format::OpenAiChat {
            "output_tokens"
        } else {
            "completion_tokens"
        }] = json!(output);
    }
    if from == Format::OpenAiChat {
        if let Some(read) = read {
            result["cache_read_input_tokens"] = json!(read);
        }
        if let Some(write) = write {
            result["cache_creation_input_tokens"] = json!(write);
        }
        if let Some(reasoning) = reasoning {
            result["output_tokens_details"] = json!({"thinking_tokens":reasoning});
        }
    } else {
        if read.is_some() || write.is_some() {
            let mut details = json!({});
            if let Some(read) = read {
                details["cached_tokens"] = json!(read);
            }
            if let Some(write) = write {
                details["cache_creation_tokens"] = json!(write);
            }
            result["prompt_tokens_details"] = details;
        }
        if let Some(reasoning) = reasoning {
            result["completion_tokens_details"] = json!({"reasoning_tokens":reasoning});
        }
        if let (Some(i), Some(o)) = (input, output) {
            result["total_tokens"] =
                json!(i.checked_add(o).ok_or_else(|| invalid("usage overflow"))?);
        }
    }
    Ok(result)
}
fn optional(v: Option<&Value>) -> Result<Option<u64>> {
    v.map(uint).transpose()
}
trait CountTranspose {
    fn transpose_count(self) -> Result<Option<u64>>;
}
impl CountTranspose for Option<Option<u64>> {
    fn transpose_count(self) -> Result<Option<u64>> {
        match self {
            None => Ok(None),
            Some(Some(n)) => Ok(Some(n)),
            Some(None) => Err(invalid("input/cache mismatch or overflow")),
        }
    }
}
