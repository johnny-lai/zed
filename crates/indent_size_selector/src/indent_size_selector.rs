mod indentation;

use editor::Editor;
use fuzzy::{match_strings, StringMatch, StringMatchCandidate};
use gpui::{
    actions, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, WeakEntity, Window,
};
pub use indentation::Indentation;
use language::{language_settings::language_settings, IndentKind, IndentSize};
use picker::{Picker, PickerDelegate};
use settings::{LocalSettingsKind, SettingsStore};
use text::Point;
use ui::{prelude::*, HighlightedLabel, ListItem};
use util::ResultExt;
use workspace::{ModalView, Workspace};

actions!(indent_size_selector, [Toggle]);

pub fn init(cx: &mut App) {
    cx.observe_new(IndentSizeSelector::register).detach();
}

pub struct IndentSizeSelector {
    picker: Entity<Picker<IndentSizeSelectorDelegate>>,
}

impl IndentSizeSelector {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(move |workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, window, cx);
        });
    }

    fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<()> {
        let editor = workspace.active_item(cx)?.act_as::<Editor>(cx)?;

        workspace.toggle_modal(window, cx, move |window, cx| {
            IndentSizeSelector::new(editor, window, cx)
        });
        Some(())
    }

    fn new(editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = IndentSizeSelectorDelegate::new(cx.entity().downgrade(), editor);

        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for IndentSizeSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for IndentSizeSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for IndentSizeSelector {}
impl ModalView for IndentSizeSelector {}

pub struct IndentSizeSelectorDelegate {
    indent_size_selector: WeakEntity<IndentSizeSelector>,
    editor: Entity<Editor>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl IndentSizeSelectorDelegate {
    fn new(indent_size_selector: WeakEntity<IndentSizeSelector>, editor: Entity<Editor>) -> Self {
        Self {
            indent_size_selector,
            editor,
            candidates: Vec::from([
                StringMatchCandidate::new(16, "Toggle Spaces/Tabs"),
                StringMatchCandidate::new(2, "2 spaces"),
                StringMatchCandidate::new(4, "4 spaces"),
                StringMatchCandidate::new(8, "8 spaces"),
            ]),
            matches: Vec::new(),
            selected_index: 0,
        }
    }
}

impl PickerDelegate for IndentSizeSelectorDelegate {
    type ListItem = ListItem;

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

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> std::sync::Arc<str> {
        "Set Indentation".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let indent_size = mat.candidate_id;

            let editor = self.editor.downgrade();
            let _ = editor.update(cx, |editor, cx| {
                // TODO: Handle editors without files
                // If there is no file, then there is no language?
                // Do indentation settings apply for:
                // 1. the language?
                // 2. the file?
                // 3. the editor?
                // 4. the language of the project?
                if let Some(file) = editor.file_at(Point::zero(), cx) {
                    let _ = cx.update_global(|store: &mut SettingsStore, cx| {
                        let worktree_id = file.worktree_id(cx);
                        let path = file.path().clone();
                        let config = format!("[/**]\nindent_size = {indent_size}\nindent_style = space\ntab_width={indent_size}");
                        let _ = store
                            .set_local_settings(
                                worktree_id,
                                path,
                                LocalSettingsKind::Editorconfig,
                                Some(&config),
                                cx,
                            )
                            .inspect_err(|e| log::error!("set_indent failed: {e}"));
                    });
                }
            });
        }

        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.indent_size_selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = &self.matches[ix];
        self.candidates
            .iter()
            .find(|x| x.id == mat.candidate_id)
            .map(|c| {
                ListItem::new(ix)
                    .inset(true)
                    .toggle_state(selected)
                    .child(HighlightedLabel::new(
                        c.string.clone(),
                        mat.positions.clone(),
                    ))
            })
            .take()
    }
}

fn read_indent_size(editor: Entity<Editor>, cx: &App) -> Option<IndentSize> {
    let editor = editor.read(cx);
    editor.active_excerpt(cx).and_then(|(_, buffer, _)| {
        let buffer = buffer.read(cx);
        buffer.language().map(|language| {
            let file = buffer.file();
            let settings = language_settings(Some(language.name()), file, cx);

            IndentSize {
                len: settings.tab_size.into(),
                kind: match settings.hard_tabs {
                    false => IndentKind::Space,
                    true => IndentKind::Tab,
                },
            }
        })
    })
}
