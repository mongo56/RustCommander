//! Thin scrollbars. The file lists, Find files results, viewer and diff are
//! virtualised (only the rows on screen are built), so they get the custom
//! [`Scrollbar`] widget, drawn from a first row / visible rows / total rows
//! triple; lists in an iced `scrollable` get the same look from [`thin`].
//! Both show only when the content overflows.

use crate::theme;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::widget::scrollable::{self, Direction, Scrollable};
use iced::widget::{container, row, stack, Space};
use iced::{event, mouse, Background, Border, Color, Element, Event, Length, Rectangle, Size, Theme};

/// Width of the bar (its hit area).
pub const WIDTH: f32 = 8.0;
/// Width of the drawn thumb.
const THUMB: f32 = 6.0;
/// Shortest thumb, so it stays easy to grab in huge lists.
const MIN_THUMB: f32 = 24.0;

/// Thumb start and length in pixels on a track `track` pixels long, for a
/// view of `visible` units starting at `offset` out of `total`; `None` when
/// everything fits.
pub fn thumb(offset: f32, visible: f32, total: f32, track: f32) -> Option<(f32, f32)> {
    if total <= visible || track <= 0.0 {
        return None;
    }
    let len = (track * visible / total).max(MIN_THUMB).min(track);
    let max = total - visible;
    Some(((track - len) * (offset.clamp(0.0, max) / max), len))
}

/// The offset that puts the thumb's start at `pos` (the inverse of [`thumb`]).
pub fn offset_at(pos: f32, visible: f32, total: f32, track: f32) -> f32 {
    let Some((_, len)) = thumb(0.0, visible, total, track) else { return 0.0 };
    let room = track - len;
    if room <= 0.0 {
        return 0.0;
    }
    (pos / room).clamp(0.0, 1.0) * (total - visible)
}

/// Offset after a click on the track at `y`: a page towards the click.
pub fn page_towards(y: f32, offset: f32, visible: f32, total: f32, track: f32) -> f32 {
    let Some((pos, len)) = thumb(offset, visible, total, track) else { return offset };
    let max = total - visible;
    if y < pos {
        (offset - visible).clamp(0.0, max)
    } else if y >= pos + len {
        (offset + visible).clamp(0.0, max)
    } else {
        offset
    }
}

/// A vertical scrollbar for a virtualised list.
pub struct Scrollbar<'a, Message> {
    offset: f32,
    visible: f32,
    total: f32,
    on_scroll: Box<dyn Fn(f32) -> Message + 'a>,
}

/// `content` with a [`Scrollbar`] over its right edge (in the rows' right
/// padding, so columns don't shift) when `total` units don't fit in
/// `visible`; `on_scroll` gets the new offset (not rounded).
pub fn attach<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    offset: f32,
    visible: f32,
    total: f32,
    on_scroll: impl Fn(f32) -> Message + 'a,
) -> Element<'a, Message> {
    if total <= visible {
        return content.into();
    }
    let bar = Scrollbar { offset, visible, total, on_scroll: Box::new(on_scroll) };
    stack![content.into(), row![Space::with_width(Length::Fill), Element::new(bar)]].into()
}

#[derive(Debug, Default)]
struct State {
    /// While dragging the thumb: where on it the press was.
    grab: Option<f32>,
}

impl<Message, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer> for Scrollbar<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(WIDTH), Length::Fill)
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, WIDTH, Length::Fill)
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let Some((pos, _)) = thumb(self.offset, self.visible, self.total, bounds.height) else {
            state.grab = None;
            return event::Status::Ignored;
        };
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(at) = cursor.position_in(bounds) else { return event::Status::Ignored };
                let paged = page_towards(at.y, self.offset, self.visible, self.total, bounds.height);
                if paged == self.offset {
                    state.grab = Some(at.y - pos);
                } else {
                    shell.publish((self.on_scroll)(paged));
                }
                event::Status::Captured
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(grab) = state.grab else { return event::Status::Ignored };
                let at = offset_at(position.y - bounds.y - grab, self.visible, self.total, bounds.height);
                if at != self.offset {
                    shell.publish((self.on_scroll)(at));
                }
                event::Status::Captured
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.grab.is_some() => {
                state.grab = None;
                event::Status::Captured
            }
            _ => event::Status::Ignored,
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        // Not `None` over the bar, so rows below it don't show hover.
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some((pos, len)) = thumb(self.offset, self.visible, self.total, bounds.height) else { return };
        let active = tree.state.downcast_ref::<State>().grab.is_some() || cursor.is_over(bounds);
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x: bounds.x + (WIDTH - THUMB) / 2.0,
                    y: bounds.y + pos,
                    width: THUMB,
                    height: len,
                },
                border: Border { radius: (THUMB / 2.0).into(), ..Border::default() },
                ..renderer::Quad::default()
            },
            Background::Color(thumb_color(active)),
        );
    }
}

/// Thumb colour: the muted text colour, stronger while hovered or dragged.
fn thumb_color(active: bool) -> Color {
    Color { a: if active { 0.8 } else { 0.4 }, ..theme::c().muted }
}

/// `content` in a vertical `scrollable` with the thin scrollbar.
pub fn thin<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Scrollable<'a, Message> {
    Scrollable::new(content).direction(Direction::Vertical(bar())).style(style)
}

/// The thin scrollbar of an iced `scrollable` (both ways for [`Direction::Both`]).
pub fn bar() -> scrollable::Scrollbar {
    scrollable::Scrollbar::new().width(WIDTH).scroller_width(THUMB).margin(0).spacing(2)
}

/// `scrollable` style matching [`Scrollbar`].
pub fn style(_theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let (vertical, horizontal) = match status {
        scrollable::Status::Active => (false, false),
        scrollable::Status::Hovered { is_vertical_scrollbar_hovered: v, is_horizontal_scrollbar_hovered: h } => (v, h),
        scrollable::Status::Dragged { is_vertical_scrollbar_dragged: v, is_horizontal_scrollbar_dragged: h } => (v, h),
    };
    let rail = |active: bool| scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            color: thumb_color(active),
            border: Border { radius: (THUMB / 2.0).into(), ..Border::default() },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail(vertical),
        horizontal_rail: rail(horizontal),
        gap: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thumb_when_everything_fits() {
        assert_eq!(thumb(0.0, 30.0, 30.0, 300.0), None);
        assert_eq!(thumb(0.0, 30.0, 10.0, 300.0), None);
        assert_eq!(thumb(0.0, 30.0, 100.0, 0.0), None);
    }

    #[test]
    fn thumb_length_and_position_follow_the_view() {
        // A third of the list on a 300 px track: a 100 px thumb.
        assert_eq!(thumb(0.0, 10.0, 30.0, 300.0), Some((0.0, 100.0)));
        assert_eq!(thumb(10.0, 10.0, 30.0, 300.0), Some((100.0, 100.0)));
        assert_eq!(thumb(20.0, 10.0, 30.0, 300.0), Some((200.0, 100.0)));
        // Past the end clamps to the bottom.
        assert_eq!(thumb(50.0, 10.0, 30.0, 300.0), Some((200.0, 100.0)));
    }

    #[test]
    fn huge_lists_keep_a_grabbable_thumb() {
        let (pos, len) = thumb(0.0, 30.0, 100_000.0, 300.0).unwrap();
        assert_eq!((pos, len), (0.0, MIN_THUMB));
        let (pos, _) = thumb(100_000.0 - 30.0, 30.0, 100_000.0, 300.0).unwrap();
        assert_eq!(pos, 300.0 - MIN_THUMB);
    }

    #[test]
    fn offset_at_inverts_thumb() {
        for offset in [0.0, 5.0, 12.5, 20.0] {
            let (pos, _) = thumb(offset, 10.0, 30.0, 300.0).unwrap();
            assert!((offset_at(pos, 10.0, 30.0, 300.0) - offset).abs() < 1e-4);
        }
        // Dragged past either end.
        assert_eq!(offset_at(-50.0, 10.0, 30.0, 300.0), 0.0);
        assert_eq!(offset_at(500.0, 10.0, 30.0, 300.0), 20.0);
        // Nothing to scroll.
        assert_eq!(offset_at(100.0, 30.0, 30.0, 300.0), 0.0);
    }

    #[test]
    fn track_clicks_page_towards_the_click() {
        // Thumb at 100..200.
        assert_eq!(page_towards(50.0, 10.0, 10.0, 30.0, 300.0), 0.0);
        assert_eq!(page_towards(250.0, 10.0, 10.0, 30.0, 300.0), 20.0);
        assert_eq!(page_towards(150.0, 10.0, 10.0, 30.0, 300.0), 10.0);
        // Clamped at the ends.
        assert_eq!(page_towards(250.0, 15.0, 10.0, 30.0, 300.0), 20.0);
        assert_eq!(page_towards(10.0, 5.0, 4.0, 30.0, 300.0), 1.0);
    }
}
