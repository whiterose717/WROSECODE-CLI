#[derive(Clone, Copy, Debug)]
pub enum Harness {
    Minimal,
    Swe,
    Claude,
}

impl Harness {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "minimal" => Some(Self::Minimal),
            "swe" => Some(Self::Swe),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Swe => "swe",
            Self::Claude => "claude",
        }
    }
    pub fn prompt(self) -> &'static str {
        match self {
            Self::Minimal => "Be concise. Use a single tool call at a time when needed. Read a file before editing unless its exact current contents are in context.",
            Self::Swe => "Investigate the repository, state a short plan, batch independent search calls, edit carefully, and verify. Read a file before editing unless its exact current contents are in context.",
            Self::Claude => "You are a coding agent. Ground claims in tool results. Batch independent structured tool calls. Read a file before editing unless its exact current contents are in context. Make exact edits and verify them.",
        }
    }
}
