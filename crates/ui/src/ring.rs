//! `Ring`: a stack of fixed-height rows whose children are given in *slot*
//! order rather than screen order. The file list puts entry `i` in slot
//! `i % rows`, so when the list scrolls by a few rows only those slots get a
//! new entry; the rest keep their widget state, and with it their shaped
//! text (iced re-shapes a `text` only when its content changes). With a
//! plain `Column` every row's text changes on every scroll step, so all of
//! them were shaped again each frame. iced's `keyed_column` doesn't help: it
//! only finds single insertions/removals, not a shift of every row.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, renderer, Clipboard, Shell};
use iced::{event, mouse, Element, Event, Length, Rectangle, Size, Vector};

pub struct Ring<'a, Message, Theme, Renderer> {
    children: Vec<Element<'a, Message, Theme, Renderer>>,
    row_h: f32,
    /// Slot shown on the top row; slot `k` is drawn on row
    /// `(k + len - top) % len`.
    top: usize,
}

impl<'a, Message, Theme, Renderer> Ring<'a, Message, Theme, Renderer> {
    /// `children` in slot order; slot `top` goes on the first row.
    pub fn new(children: Vec<Element<'a, Message, Theme, Renderer>>, row_h: f32, top: usize) -> Self {
        let top = if children.is_empty() { 0 } else { top % children.len() };
        Self { children, row_h, top }
    }
}

/// Screen row of slot `slot` in a ring of `len` slots whose top row shows `top`.
fn row_of(slot: usize, top: usize, len: usize) -> usize {
    (slot + len - top) % len
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Ring<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let max = limits.max();
        let row = layout::Limits::new(Size::ZERO, Size::new(max.width, self.row_h));
        let len = self.children.len();
        let nodes = self
            .children
            .iter()
            .zip(&mut tree.children)
            .enumerate()
            .map(|(slot, (child, state))| {
                let y = row_of(slot, self.top, len) as f32 * self.row_h;
                child.as_widget().layout(state, renderer, &row).move_to((0.0, y))
            })
            .collect();
        layout::Node::with_children(max, nodes)
    }

    fn operate(&self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        operation.container(None, layout.bounds(), &mut |operation| {
            for ((child, state), layout) in self.children.iter().zip(&mut tree.children).zip(layout.children()) {
                child.as_widget().operate(state, layout, renderer, operation);
            }
        });
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
        self.children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .map(|((child, state), layout)| {
                child.as_widget_mut().on_event(state, event.clone(), layout, cursor, renderer, clipboard, shell, viewport)
            })
            .fold(event::Status::Ignored, event::Status::merge)
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), layout)| child.as_widget().mouse_interaction(state, layout, cursor, viewport, renderer))
            .max()
            .unwrap_or_default()
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
        let Some(clip) = layout.bounds().intersection(viewport) else { return };
        for ((child, state), layout) in self.children.iter().zip(&tree.children).zip(layout.children()) {
            child.as_widget().draw(state, renderer, theme, style, layout, cursor, &clip);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(&mut self.children, tree, layout, renderer, translation)
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<Ring<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: Ring<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::row_of;

    #[test]
    fn slots_map_to_rows_in_order() {
        // Entries 7..12 in a ring of 5: entry i sits in slot i % 5, so the
        // top row shows slot 2 and the rows read 7, 8, 9, 10, 11.
        let rows: Vec<usize> = (7..12).map(|i| row_of(i % 5, 7 % 5, 5)).collect();
        assert_eq!(rows, vec![0, 1, 2, 3, 4]);
    }
}
