use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WriteStdinInput {
    pub session_id: i32,
    #[serde(default)]
    pub chars: String,
    #[serde(default = "default_write_stdin_yield_time_ms")]
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl<'de> Deserialize<'de> for WriteStdinInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawWriteStdinInput {
            session_id: i32,
            #[serde(default)]
            chars: String,
            #[serde(default)]
            yield_time_ms: Option<u64>,
            #[serde(default)]
            max_output_tokens: Option<usize>,
        }

        let raw = RawWriteStdinInput::deserialize(deserializer)?;
        let yield_time_ms = raw.yield_time_ms.unwrap_or_else(|| {
            if raw.chars.is_empty() {
                default_empty_poll_yield_time_ms()
            } else {
                default_write_stdin_yield_time_ms()
            }
        });
        Ok(Self {
            session_id: raw.session_id,
            chars: raw.chars,
            yield_time_ms,
            max_output_tokens: raw.max_output_tokens,
        })
    }
}

impl WriteStdinInput {
    #[must_use]
    pub fn poll(session_id: i32) -> Self {
        Self {
            session_id,
            chars: String::new(),
            yield_time_ms: default_empty_poll_yield_time_ms(),
            max_output_tokens: None,
        }
    }
}

const fn default_write_stdin_yield_time_ms() -> u64 {
    250
}

const fn default_empty_poll_yield_time_ms() -> u64 {
    5_000
}
