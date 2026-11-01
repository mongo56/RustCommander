//! Selecting part of a `text_input`'s text.
//!
//! iced 0.13's `text_input` can only select everything from the outside
//! (`operation::TextInput` has `move_cursor_to` and `select_all`; the
//! cursor's `select_range` is crate-private). `SelectRange` wraps the input
//! and, once the input has focus, moves its cursor with an operation and then
//! feeds it Shift+Right / Shift+End key presses, which the input handles as a
//! selection. Each request carries a generation (`next_generation`) so it is
//! applied once; the wrapper's tree state remembers the last one applied.
//!
//! Positions are grapheme indices, like the input's own `Value`.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::operation::{Focusable, Operation, TextInput};
use iced::advanced::widget::{tree, Id, Tree, Widget};
use iced::advanced::{mouse, overlay, renderer, Clipboard, Shell};
use iced::keyboard::key::{Code, Named, Physical};
use iced::keyboard::{self, Key, Location, Modifiers};
use iced::{event, Element, Event, Length, Rectangle, Size, Vector};
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_segmentation::UnicodeSegmentation;

/// A fresh id for a selection request.
pub fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Which part of a file name is selected while renaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Stem,
    Extension,
    All,
}

impl Part {
    /// What a rename starts with: the stem of a file, all of a folder.
    pub fn initial(is_dir: bool) -> Self {
        if is_dir { Part::All } else { Part::Stem }
    }

    /// The next part when F2 is pressed again: stem → extension → all → stem
    /// (parts that don't exist are skipped).
    pub fn next(self, name: &str, is_dir: bool) -> Self {
        let order = [Part::Stem, Part::Extension, Part::All];
        let at = order.iter().position(|p| *p == self).unwrap_or(0);
        (1..=3)
            .map(|i| order[(at + i) % 3])
            .find(|p| p.range(name, is_dir).is_some_and(|r| r != self.range(name, is_dir).unwrap_or((0, 0))))
            .unwrap_or(self)
    }

    /// The part's (start, end) grapheme range in `name`, `None` if it has
    /// none. Extensions follow `Entry::split_name`: after the last dot,
    /// unless it's the first character; folders have none.
    pub fn range(self, name: &str, is_dir: bool) -> Option<(usize, usize)> {
        let len = name.graphemes(true).count();
        let dot = if is_dir { None } else { name.rfind('.').filter(|&i| i > 0) };
        let stem = dot.map_or(len, |i| name[..i].graphemes(true).count());
        match self {
            Part::All => Some((0, len)),
            Part::Stem => Some((0, stem)),
            Part::Extension => (dot.is_some() && stem + 1 < len).then_some((stem + 1, len)),
        }
    }
}

/// Wraps a text input (`id`) and selects `range` in it once per `generation`.
pub struct SelectRange<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    id: Id,
    generation: u64,
    range: (usize, usize),
}

impl<'a, Message, Theme, Renderer> SelectRange<'a, Message, Theme, Renderer> {
    pub fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        id: impl Into<Id>,
        generation: u64,
        range: (usize, usize),
    ) -> Self {
        Self { content: content.into(), id: id.into(), generation, range }
    }
}

/// Tree state: the last generation applied and the modifiers the input
/// believes are held (restored after the fake Shift).
#[derive(Default)]
struct State {
    applied: u64,
    modifiers: Modifiers,
}

/// Reads the input's focus and optionally moves its cursor.
struct Cursor<'a> {
    id: &'a Id,
    focused: bool,
    move_to: Option<usize>,
}

impl Operation for Cursor<'_> {
    fn container(&mut self, _id: Option<&Id>, _bounds: Rectangle, children: &mut dyn FnMut(&mut dyn Operation)) {
        children(self);
    }

    fn focusable(&mut self, state: &mut dyn Focusable, id: Option<&Id>) {
        if id == Some(self.id) {
            self.focused = state.is_focused();
        }
    }

    fn text_input(&mut self, state: &mut dyn TextInput, id: Option<&Id>) {
        if let (Some(at), true) = (self.move_to, id == Some(self.id)) {
            state.move_cursor_to(at);
        }
    }
}

fn key_press(named: Named, code: Code, modifiers: Modifiers) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: Key::Named(named),
        modified_key: Key::Named(named),
        physical_key: Physical::Code(code),
        location: Location::Standard,
        modifiers,
        text: None,
    })
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for SelectRange<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget().layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn operate(&self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> event::Status {
        let state = tree.state.downcast_mut::<State>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(m)) = &event {
            state.modifiers = *m;
        }
        if state.applied != self.generation {
            let mut probe = Cursor { id: &self.id, focused: false, move_to: None };
            self.content.as_widget().operate(&mut tree.children[0], layout, renderer, &mut probe);
            // Focus arrives through a task after the first view; wait for it.
            if probe.focused {
                state.applied = self.generation;
                let restore = state.modifiers;
                let (start, end) = self.range;
                let mut select = Cursor { id: &self.id, focused: false, move_to: Some(start) };
                self.content.as_widget().operate(&mut tree.children[0], layout, renderer, &mut select);
                let mut feed = |event: Event| {
                    let _ = self.content.as_widget_mut().on_event(
                        &mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport,
                    );
                };
                if end > start {
                    feed(Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::SHIFT)));
                    for _ in start..end {
                        feed(key_press(Named::ArrowRight, Code::ArrowRight, Modifiers::SHIFT));
                    }
                    feed(Event::Keyboard(keyboard::Event::ModifiersChanged(restore)));
                }
            }
        }
        self.content.as_widget_mut().on_event(
            &mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport,
        )
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<SelectRange<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: SelectRange<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::Part::{self, All, Extension, Stem};

    #[test]
    fn ranges_skip_the_extension() {
        assert_eq!(Stem.range("report.final.txt", false), Some((0, 12)));
        assert_eq!(Extension.range("report.final.txt", false), Some((13, 16)));
        assert_eq!(All.range("report.final.txt", false), Some((0, 16)));
        // Folders, dotfiles, no dot, trailing dot.
        assert_eq!(Stem.range("my.folder", true), Some((0, 9)));
        assert_eq!(Extension.range("my.folder", true), None);
        assert_eq!(Stem.range(".gitignore", false), Some((0, 10)));
        assert_eq!(Extension.range(".gitignore", false), None);
        assert_eq!(Stem.range("README", false), Some((0, 6)));
        assert_eq!(Stem.range("foo.", false), Some((0, 3)));
        assert_eq!(Extension.range("foo.", false), None);
        assert_eq!(All.range("", false), Some((0, 0)));
    }

    #[test]
    fn ranges_count_graphemes_not_bytes() {
        // "ž" is 2 bytes, "e\u{301}" is 2 chars but one grapheme.
        assert_eq!(Stem.range("žaba.txt", false), Some((0, 4)));
        assert_eq!(Extension.range("žaba.txt", false), Some((5, 8)));
        assert_eq!(Stem.range("cafe\u{301}.ünï", false), Some((0, 4)));
        assert_eq!(Extension.range("cafe\u{301}.ünï", false), Some((5, 8)));
        assert_eq!(Stem.range("日本語.文書", false), Some((0, 3)));
        assert_eq!(All.range("日本語.文書", false), Some((0, 6)));
    }

    #[test]
    fn f2_cycles_through_the_parts() {
        assert_eq!(Part::initial(false), Stem);
        assert_eq!(Part::initial(true), All);
        let name = "a.txt";
        assert_eq!(Stem.next(name, false), Extension);
        assert_eq!(Extension.next(name, false), All);
        assert_eq!(All.next(name, false), Stem);
        // No extension: stem and all are the same range, nothing to cycle.
        assert_eq!(Stem.next("README", false), Stem);
        assert_eq!(All.next("folder.x", true), All);
    }
}
