use std::sync::Arc;

use acp_thread::{AcpThread, AgentThreadEntry, MentionUri};
use agent_client_protocol::v2::schema as acp_v2;
use gpui::{Action as _, AnyElement, App, DismissEvent, Task, WeakEntity};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathStyle};

use super::thread_view::ThreadView;

/// Long prompts (e.g. pasted logs) are searched in full, but only this prefix
/// is shaped for display; rows truncate visually well before it.
const MAX_LABEL_CHARS: usize = 240;

struct PromptOutlineItem {
    entry_ix: usize,
    text: String,
    label: SharedString,
    label_prefix_len: usize,
    /// Whether the agent accepts forking the thread before this prompt.
    can_fork: bool,
}

struct PromptOutlineMatch {
    item_ix: usize,
    highlight_positions: Vec<usize>,
}

pub(crate) struct PromptOutlineDelegate {
    thread_view: WeakEntity<ThreadView>,
    items: Vec<PromptOutlineItem>,
    current_item_ix: Option<usize>,
    matches: Vec<PromptOutlineMatch>,
    selected_index: usize,
}

impl PromptOutlineDelegate {
    /// `scroll_top_entry_ix` is the entry at the top of the thread's viewport;
    /// the prompt at or above it is treated as the one currently being read.
    pub(crate) fn new(
        thread_view: WeakEntity<ThreadView>,
        thread: &AcpThread,
        scroll_top_entry_ix: usize,
        cx: &App,
    ) -> Self {
        let path_style = thread.project().read(cx).path_style(cx);
        let items: Vec<_> = thread
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| thread.is_user_authored_scroll_target(entry))
            .map(|(entry_ix, entry)| {
                let text = entry_text(entry, path_style, cx);
                let label_prefix = util::truncate(&text, MAX_LABEL_CHARS);
                let label_prefix_len = label_prefix.len();
                let label = if label_prefix_len < text.len() {
                    format!("{label_prefix}…").into()
                } else {
                    text.clone().into()
                };
                PromptOutlineItem {
                    entry_ix,
                    text,
                    label,
                    label_prefix_len,
                    can_fork: thread.fork_cursor_before_entry(entry_ix).is_some(),
                }
            })
            .collect();
        let current_item_ix = items
            .iter()
            .rposition(|item| item.entry_ix <= scroll_top_entry_ix);

        Self {
            thread_view,
            items,
            current_item_ix,
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    fn selected_item(&self) -> Option<&PromptOutlineItem> {
        self.matches
            .get(self.selected_index)
            .and_then(|prompt_match| self.items.get(prompt_match.item_ix))
    }
}

impl PickerDelegate for PromptOutlineDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "prompt outline"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search prompts…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching prompts".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(item_ix, item)| {
                let highlight_positions = if query.is_empty() {
                    Vec::new()
                } else {
                    case_insensitive_match_positions(&item.text, query)?
                };
                Some(PromptOutlineMatch {
                    item_ix,
                    highlight_positions,
                })
            })
            .collect();
        self.selected_index = self
            .current_item_ix
            .and_then(|current_item_ix| {
                self.matches
                    .iter()
                    .rposition(|prompt_match| prompt_match.item_ix <= current_item_ix)
            })
            .unwrap_or(0);
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let entry_ix = item.entry_ix;
        let fork = secondary && item.can_fork;
        self.thread_view
            .update(cx, |thread_view, cx| {
                if fork {
                    thread_view.fork_before_entry(entry_ix, window, cx);
                } else {
                    thread_view.scroll_to_entry(entry_ix, cx);
                }
            })
            .log_err();
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if !self.items.iter().any(|item| item.can_fork) {
            return None;
        }
        let selected_can_fork = self.selected_item().is_some_and(|item| item.can_fork);
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("fork-before-prompt", "Fork Before Prompt")
                        .disabled(!selected_can_fork)
                        .key_binding(
                            KeyBinding::for_action(&menu::SecondaryConfirm, cx)
                                .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                        )
                        .tooltip(Tooltip::text(
                            "Start a new thread with the history before this prompt, \
                             and the prompt ready to edit and send again",
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .into_any(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let prompt_match = self.matches.get(ix)?;
        let item = self.items.get(prompt_match.item_ix)?;
        let is_current = self.current_item_ix == Some(prompt_match.item_ix);

        let digit_count = self.items.len().to_string().len();
        let highlight_positions = prompt_match
            .highlight_positions
            .iter()
            .copied()
            .filter(|&position| position < item.label_prefix_len)
            .collect();

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    h_flex()
                        .min_w(rems(0.5 * digit_count as f32))
                        .justify_end()
                        .child(
                            Label::new((prompt_match.item_ix + 1).to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    HighlightedLabel::new(item.label.clone(), highlight_positions)
                        .color(if is_current {
                            Color::Accent
                        } else {
                            Color::Default
                        })
                        .truncate(),
                ),
        )
    }
}

/// Flattens a prompt to a single line, rendering mentions as `@name` the way
/// they read in the message editor.
fn entry_text(entry: &AgentThreadEntry, path_style: PathStyle, cx: &App) -> String {
    let mut text = String::new();
    let mut push_words = |part: &str| {
        for word in part.split_whitespace() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(word);
        }
    };

    match entry {
        AgentThreadEntry::UserMessage(message) => {
            for block in message.content.source_blocks() {
                match block {
                    acp_v2::ContentBlock::Text(content) => push_words(&content.text),
                    acp_v2::ContentBlock::ResourceLink(link) => {
                        push_words(&format!("@{}", link.name));
                    }
                    acp_v2::ContentBlock::Resource(resource) => {
                        let uri = match &resource.resource {
                            acp_v2::EmbeddedResourceResource::TextResourceContents(contents) => {
                                &contents.uri
                            }
                            acp_v2::EmbeddedResourceResource::BlobResourceContents(contents) => {
                                &contents.uri
                            }
                            _ => continue,
                        };
                        let name = MentionUri::parse(uri, path_style)
                            .map_or_else(|_| uri.clone(), |mention| mention.name());
                        push_words(&format!("@{name}"));
                    }
                    acp_v2::ContentBlock::Image(_) => push_words("[Image]"),
                    _ => {}
                }
            }
        }
        AgentThreadEntry::ToolCall(call) => push_words(call.label.read(cx).source()),
        _ => {}
    }

    text
}

/// Returns the byte offsets of the characters in the first occurrence of
/// `query` within `text`, ignoring case.
fn case_insensitive_match_positions(text: &str, query: &str) -> Option<Vec<usize>> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Some(Vec::new());
    }
    let text: Vec<(usize, char)> = text.char_indices().collect();
    text.windows(query.len()).find_map(|window| {
        window
            .iter()
            .zip(&query)
            .all(|((_, text_char), query_char)| {
                text_char.to_lowercase().eq(query_char.to_lowercase())
            })
            .then(|| window.iter().map(|(offset, _)| *offset).collect())
    })
}

#[cfg(test)]
mod tests {
    use super::case_insensitive_match_positions;

    #[test]
    fn test_case_insensitive_match_positions() {
        assert_eq!(
            case_insensitive_match_positions("Review the PR", "pr"),
            Some(vec![11, 12])
        );
        assert_eq!(
            case_insensitive_match_positions("再度確認を頼む", "確認"),
            Some(vec![6, 9])
        );
        assert_eq!(
            case_insensitive_match_positions("short", "longer query"),
            None
        );
        assert_eq!(case_insensitive_match_positions("abc", "x"), None);
        assert_eq!(
            case_insensitive_match_positions("abc", ""),
            Some(Vec::new())
        );
    }
}
