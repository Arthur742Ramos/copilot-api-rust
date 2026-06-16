pub const COMPACT_REQUEST: i32 = 1;
pub const COMPACT_AUTO_CONTINUE: i32 = 2;

pub const COMPACT_SYSTEM_PROMPT_START: &str =
    "You are a helpful AI assistant tasked with summarizing conversations";
pub const COMPACT_OPENCODE_SYSTEM_PROMPT_START: &str =
    "You are an anchored context summarization assistant for coding sessions.";
pub const COMPACT_SYSTEM_PROMPT_STARTS: [&str; 2] = [
    COMPACT_SYSTEM_PROMPT_START,
    COMPACT_OPENCODE_SYSTEM_PROMPT_START,
];
pub const COMPACT_TEXT_ONLY_GUARD: &str =
    "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
pub const COMPACT_SUMMARY_PROMPT_START: &str =
    "Your task is to create a detailed summary of the conversation so far";
pub const COMPACT_AUTO_CONTINUE_CLAUDE_CODE_PROMPT_START: &str =
    "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";
pub const COMPACT_AUTO_CONTINUE_OPENCODE_PROMPT_START: &str =
    "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.";
pub const COMPACT_AUTO_CONTINUE_OPENCODE_PROMPT_START2: &str =
    "The previous request exceeded the provider's size limit due to large media attachments. The conversation was compacted and media files were removed from context.";
pub const COMPACT_AUTO_CONTINUE_PROMPT_STARTS: [&str; 3] = [
    COMPACT_AUTO_CONTINUE_CLAUDE_CODE_PROMPT_START,
    COMPACT_AUTO_CONTINUE_OPENCODE_PROMPT_START,
    COMPACT_AUTO_CONTINUE_OPENCODE_PROMPT_START2,
];
pub const COMPACT_MESSAGE_SECTIONS: [&str; 2] = ["Pending Tasks:", "Current Work:"];

/// CompactType is represented as i32: 0 (none), 1 (request), 2 (auto-continue).
pub type CompactType = i32;
