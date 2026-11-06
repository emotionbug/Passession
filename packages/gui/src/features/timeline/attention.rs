//! Port of `agent_attention.dart` (hooks/use-agent-attention-clear.ts): clears
//! the attention flag of the agent a panel shows when its pane gains focus (or
//! the app resumes into it), when its input is focused, when a prompt is sent,
//! and when the focused panel is left.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    FocusEntry,
    InputFocus,
    PromptSend,
    AgentBlur,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub agent_id: Option<String>,
    pub connected: bool,
    pub requires_attention: bool,
    pub attention_reason: Option<String>,
    pub screen_focused: bool,
    pub app_visible: bool,
}

/// utils/agent-attention.ts shouldClearAgentAttention.
pub fn should_clear(state: &State, trigger: Trigger, deferred_focus_entry: bool) -> bool {
    let Some(id) = state.agent_id.as_deref() else { return false };
    if id.trim().is_empty() || !state.connected || !state.requires_attention {
        return false;
    }
    if state.attention_reason.as_deref() == Some("permission") {
        return false;
    }
    !(trigger == Trigger::FocusEntry && deferred_focus_entry)
}

pub struct AttentionClear {
    state: State,
    deferred_focus_entry: bool,
    prev_requires_attention: bool,
    prev_actively_viewed: bool,
    prev_screen_focused: bool,
    prev_app_visible: bool,
    /// Agent ids to send `clear_agent_attention` for.
    pub cleared: Vec<String>,
}

impl AttentionClear {
    pub fn new(initial: State) -> Self {
        let mut this = Self {
            prev_requires_attention: initial.requires_attention,
            prev_actively_viewed: initial.screen_focused && initial.app_visible,
            prev_app_visible: initial.app_visible,
            prev_screen_focused: false,
            deferred_focus_entry: false,
            state: initial,
            cleared: Vec::new(),
        };
        this.effects();
        this
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Feed every state change; a no-op for an unchanged state.
    pub fn update(&mut self, next: State) {
        if next == self.state {
            return;
        }
        self.state = next;
        self.effects();
    }

    pub fn clear_on(&mut self, trigger: Trigger) {
        self.clear(trigger);
    }

    fn effects(&mut self) {
        if !self.state.requires_attention {
            self.deferred_focus_entry = false;
        }
        let actively_viewed = self.state.screen_focused && self.state.app_visible;
        if !self.prev_requires_attention && self.state.requires_attention && self.prev_actively_viewed && actively_viewed {
            self.deferred_focus_entry = true;
        }
        self.prev_requires_attention = self.state.requires_attention;
        self.prev_actively_viewed = actively_viewed;
        let entered = !self.prev_screen_focused && self.state.screen_focused && self.state.app_visible;
        let resumed = !self.prev_app_visible && self.state.app_visible && self.state.screen_focused;
        self.prev_screen_focused = self.state.screen_focused;
        self.prev_app_visible = self.state.app_visible;
        if entered || resumed {
            self.clear(Trigger::FocusEntry);
        }
    }

    fn clear(&mut self, trigger: Trigger) {
        if !should_clear(&self.state, trigger, self.deferred_focus_entry) {
            return;
        }
        self.deferred_focus_entry = false;
        if let Some(id) = self.state.agent_id.as_deref() {
            self.cleared.push(id.trim().to_owned());
        }
    }
}
