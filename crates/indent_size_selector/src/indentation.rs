use crate::{read_indent_size, IndentSizeSelector, Toggle};
use editor::Editor;
use gpui::{Entity, Subscription, WeakEntity};
use language::{IndentKind, IndentSize};
use ui::{
    div, Button, ButtonCommon, Clickable, Context, FluentBuilder, IntoElement, LabelSize,
    ParentElement, Render, Tooltip, Window,
};
use workspace::{ItemHandle, StatusItemView, Workspace};

pub struct Indentation {
    indent_size: Option<IndentSize>,
    workspace: WeakEntity<Workspace>,
    _observe_active_editor: Option<Subscription>,
}

impl Indentation {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            indent_size: None,
            workspace: workspace.weak_handle(),
            _observe_active_editor: None,
        }
    }

    fn update_indentation(
        &mut self,
        editor: Entity<Editor>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.indent_size = read_indent_size(editor, cx);

        cx.notify();
    }
}

impl Render for Indentation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().when_some(self.indent_size, |el, indent_size| {
            let mode = match indent_size.kind {
                IndentKind::Tab => "Tab",
                IndentKind::Space => "Space",
            };
            let text = format!("{}: {}", mode, indent_size.len);
            el.child(
                Button::new("tab-size", text)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let (Some(workspace), Some(_indent_size)) =
                            (this.workspace.upgrade(), this.indent_size)
                        {
                            workspace.update(cx, |workspace, cx| {
                                IndentSizeSelector::toggle(workspace, window, cx);
                            })
                        }
                    }))
                    .tooltip(|window, cx| {
                        Tooltip::for_action("Set Indentation", &Toggle, window, cx)
                    }),
            )
        })
    }
}

impl StatusItemView for Indentation {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self._observe_active_editor =
                Some(cx.observe_in(&editor, window, Self::update_indentation));
            self.update_indentation(editor, window, cx);
        } else {
            self.indent_size = None;
            self._observe_active_editor = None;
        }

        cx.notify();
    }
}
