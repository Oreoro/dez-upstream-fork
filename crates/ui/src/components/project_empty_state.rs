use crate::prelude::*;
use gpui::FocusHandle;

#[derive(IntoElement)]
pub struct ProjectEmptyState {
    message: SharedString,
    focus_handle: FocusHandle,
}

impl ProjectEmptyState {
    pub fn new(message: impl Into<SharedString>, focus_handle: FocusHandle) -> Self {
        Self {
            message: message.into(),
            focus_handle,
        }
    }
}

impl RenderOnce for ProjectEmptyState {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id = format!("empty-state-{}", self.message);

        v_flex()
            .id(id)
            .p_4()
            .size_full()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .child(
                v_flex().w_48().max_w_full().gap_1().child(
                    div().text_center().mb_2().child(
                        Label::new(self.message)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
            )
    }
}
