//! `Follow`: draws its content next to the mouse cursor, reading the cursor
//! iced passes to `draw`. Every mouse move redraws the window anyway, so the
//! content tracks the mouse in the same frame, without waiting for a
//! `MouseMoved` message to go through `update` and a rebuilt `view` (which
//! lands a frame or more later). Used for the drag bubble.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::{mouse, renderer};
use iced::{Element, Length, Point, Rectangle, Size, Vector};

pub struct Follow<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    /// Position to use while iced has no cursor (e.g. it left the window).
    fallback: Point,
    /// Content's top-left corner relative to the cursor.
    offset: Vector,
}

impl<'a, Message, Theme, Renderer> Follow<'a, Message, Theme, Renderer> {
    /// Fills its parent (meant as the top layer of a `stack`) and draws
    /// `content` at the cursor + `offset`, kept inside the window.
    pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>, fallback: Point, offset: Vector) -> Self {
        Self { content: content.into(), fallback, offset }
    }
}

/// Top-left corner for content of `size` at `cursor + offset`, kept inside
/// `area`; flips above the cursor when it would run off the bottom.
fn place(cursor: Point, offset: Vector, size: Size, area: Rectangle) -> Point {
    let mut x = cursor.x + offset.x;
    let mut y = cursor.y + offset.y;
    if x + size.width > area.x + area.width {
        x = area.x + area.width - size.width;
    }
    if y + size.height > area.y + area.height {
        y = cursor.y - offset.y - size.height;
    }
    Point::new(x.max(area.x), y.max(area.y))
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Follow<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let max = limits.max();
        // Content laid out at our origin; `draw` moves it to the cursor.
        let child = self.content.as_widget().layout(&mut tree.children[0], renderer, &layout::Limits::new(Size::ZERO, max));
        layout::Node::with_children(max, vec![child])
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let Some(child) = layout.children().next() else { return };
        let bounds = child.bounds();
        let at = cursor.position().unwrap_or(self.fallback);
        let to = place(at, self.offset, bounds.size(), layout.bounds());
        let shift = to - bounds.position();
        renderer.with_translation(shift, |renderer| {
            // Clip in the content's own (untranslated) coordinates.
            self.content.as_widget().draw(&tree.children[0], renderer, theme, style, child, cursor, &bounds.expand(1.0));
        });
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<Follow<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: Follow<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::place;
    use iced::{Point, Rectangle, Size, Vector};

    const AREA: Rectangle = Rectangle { x: 0.0, y: 0.0, width: 800.0, height: 600.0 };
    const OFFSET: Vector = Vector::new(14.0, 16.0);

    #[test]
    fn sits_below_right_of_the_cursor() {
        assert_eq!(place(Point::new(100.0, 100.0), OFFSET, Size::new(120.0, 24.0), AREA), Point::new(114.0, 116.0));
    }

    #[test]
    fn stays_inside_the_window() {
        // Right edge: pushed left; bottom edge: flipped above the cursor.
        assert_eq!(place(Point::new(750.0, 590.0), OFFSET, Size::new(120.0, 24.0), AREA), Point::new(680.0, 550.0));
        // Wider than the window: left edge wins.
        assert_eq!(place(Point::new(10.0, 10.0), OFFSET, Size::new(900.0, 24.0), AREA).x, 0.0);
    }
}
